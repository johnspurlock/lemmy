use crate::{
  inboxes::CommunityInboxCollector,
  send::{SendActivityResult, SendRetryTask, SendSuccessInfo},
  util::{
    get_activity_cached,
    get_latest_activity_id,
    FederationQueueStateWithDomain,
    WORK_FINISHED_RECHECK_DELAY,
  },
};
use activitypub_federation::config::FederationConfig;
use anyhow::{Context, Result};
use chrono::{DateTime, Days, TimeZone, Utc};
use lemmy_api_common::{
  context::LemmyContext,
  federate_retry_sleep_duration,
  lemmy_utils::settings::structs::FederationWorkerConfig,
};
use lemmy_db_schema::{
  newtypes::ActivityId,
  source::{
    federation_queue_state::FederationQueueState,
    instance::{Instance, InstanceForm},
  },
  utils::{naive_now, ActualDbPool, DbPool},
};
use std::{collections::BinaryHeap, ops::Add, time::Duration};
use tokio::{
  sync::mpsc::{self, UnboundedSender},
  time::sleep,
};
use tokio_util::sync::CancellationToken;

/// Save state to db after this time has passed since the last state (so if the server crashes or is
/// SIGKILLed, less than X seconds of activities are resent)
#[cfg(not(test))]
static SAVE_STATE_EVERY_TIME: Duration = Duration::from_secs(60);
#[cfg(test)]
/// in test mode, we want it to save state and send it to print_stats after every send
static SAVE_STATE_EVERY_TIME: Duration = Duration::from_secs(0);
/// Maximum number of successful sends to allow out of order
const MAX_SUCCESSFULS: usize = 1000;

pub(crate) struct InstanceWorker {
  instance: Instance,
  stop: CancellationToken,
  stats_sender: UnboundedSender<FederationQueueStateWithDomain>,
  federation_lib_config: FederationConfig<LemmyContext>,
  federation_worker_config: FederationWorkerConfig,
  state: FederationQueueState,
  last_state_insert: DateTime<Utc>,
  pool: ActualDbPool,
  inbox_collector: CommunityInboxCollector,
}

impl InstanceWorker {
  pub(crate) async fn init_and_loop(
    instance: Instance,
    config: FederationConfig<LemmyContext>,
    federation_worker_config: FederationWorkerConfig,
    stop: CancellationToken,
    stats_sender: UnboundedSender<FederationQueueStateWithDomain>,
  ) -> Result<(), anyhow::Error> {
    let pool = config.to_request_data().inner_pool().clone();
    let state = FederationQueueState::load(&mut DbPool::Pool(&pool), instance.id).await?;
    let mut worker = InstanceWorker {
      inbox_collector: CommunityInboxCollector::new(
        pool.clone(),
        instance.id,
        instance.domain.clone(),
      ),
      federation_worker_config,
      instance,
      stop,
      federation_lib_config: config,
      stats_sender,
      state,
      last_state_insert: Utc.timestamp_nanos(0),
      pool,
    };
    worker.loop_until_stopped().await
  }
  /// loop fetch new activities from db and send them to the inboxes of the given instances
  /// this worker only returns if (a) there is an internal error or (b) the cancellation token is
  /// cancelled (graceful exit)
  async fn loop_until_stopped(&mut self) -> Result<()> {
    self.initial_fail_sleep().await?;
    let (mut last_sent_id, mut newest_id) = self.get_latest_ids().await?;

    // activities that have been successfully sent but
    // that are not the lowest number and thus can't be written to the database yet
    let mut successfuls = BinaryHeap::<SendSuccessInfo>::new();
    // number of activities that currently have a task spawned to send it
    let mut in_flight: i64 = 0;

    // each HTTP send will report back to this channel concurrently
    let (report_send_result, mut receive_send_result) =
      tokio::sync::mpsc::unbounded_channel::<SendActivityResult>();
    while !self.stop.is_cancelled() {
      // check if we need to wait for a send to finish before sending the next one
      // we wait if (a) the last request failed, only if a request is already in flight (not at the
      // start of the loop) or (b) if we have too many successfuls in memory or (c) if we have
      // too many in flight
      let need_wait_for_event = (in_flight != 0 && self.state.fail_count > 0)
        || successfuls.len() >= MAX_SUCCESSFULS
        || in_flight >= self.federation_worker_config.concurrent_sends_per_instance;
      if need_wait_for_event || receive_send_result.len() > 4 {
        // if len() > 0 then this does not block and allows us to write to db more often
        // if len is 0 then this means we wait for something to change our above conditions,
        // which can only happen by an event sent into the channel
        self
          .handle_send_results(&mut receive_send_result, &mut successfuls, &mut in_flight)
          .await?;
        // handle_send_results does not guarantee that we are now in a condition where we want to
        // send a new one, so repeat this check until the if no longer applies
        continue;
      } else {
        // send a new activity if there is one
        self.inbox_collector.update_communities().await?;
        let next_id_to_send = ActivityId(last_sent_id.0 + 1);
        {
          // sanity check: calculate next id to send based on the last id and the in flight requests
          let last_successful_id = self.state.last_successful_id.map(|e| e.0).context(
            "impossible: id is initialized in get_latest_ids and never returned to None",
          )?;
          let expected_next_id = last_successful_id + (successfuls.len() as i64) + in_flight + 1;
          // compare to next id based on incrementing
          if expected_next_id != next_id_to_send.0 {
            anyhow::bail!(
              "{}: next id to send is not as expected: {:?} != {:?}",
              self.instance.domain,
              expected_next_id,
              next_id_to_send
            )
          }
        }

        if next_id_to_send > newest_id {
          // lazily fetch latest id only if we have cought up
          newest_id = self.get_latest_ids().await?.1;
          if next_id_to_send > newest_id {
            if next_id_to_send > ActivityId(newest_id.0 + 1) {
              tracing::error!(
                "{}: next send id {} is higher than latest id {}+1 in database (did the db get cleared?)",
                self.instance.domain,
                next_id_to_send.0,
                newest_id.0
              );
            }
            // no more work to be done, wait before rechecking
            tokio::select! {
              () = sleep(*WORK_FINISHED_RECHECK_DELAY) => {},
              () = self.stop.cancelled() => {
                tracing::debug!("cancelled worker loop while waiting for new work")
              }
            }
            continue;
          }
        }
        in_flight += 1;
        last_sent_id = next_id_to_send;
        self
          .spawn_send_if_needed(next_id_to_send, report_send_result.clone())
          .await?;
      }
    }
    tracing::debug!("cancelled worker loop after send");

    // final update of state in db on shutdown
    self.save_and_send_state().await?;
    Ok(())
  }

  async fn initial_fail_sleep(&mut self) -> Result<()> {
    // before starting queue, sleep remaining duration if last request failed
    if self.state.fail_count > 0 {
      let last_retry = self
        .state
        .last_retry
        .context("impossible: if fail count set last retry also set")?;
      let elapsed = (Utc::now() - last_retry).to_std()?;
      let required = federate_retry_sleep_duration(self.state.fail_count);
      if elapsed >= required {
        return Ok(());
      }
      let remaining = required - elapsed;
      tracing::debug!(
        "{}: fail-sleeping for {:?} before starting queue",
        self.instance.domain,
        remaining
      );
      tokio::select! {
        () = sleep(remaining) => {},
        () = self.stop.cancelled() => {
          tracing::debug!("cancelled worker loop during initial fail sleep")
        }
      }
    }
    Ok(())
  }

  /// return the last successfully sent id and the newest activity id in the database
  /// sets last_successful_id in database if it's the first time this instance is seen
  async fn get_latest_ids(&mut self) -> Result<(ActivityId, ActivityId)> {
    let latest_id = get_latest_activity_id(&mut self.pool()).await?;
    if let Some(last) = self.state.last_successful_id {
      Ok((last, latest_id))
    } else {
      // this is the initial creation (instance first seen) of the federation queue for this
      // instance skip all past activities:
      self.state.last_successful_id = Some(latest_id);
      // save here to ensure it's not read as 0 again later if no activities have happened
      self.save_and_send_state().await?;
      Ok((latest_id, latest_id))
    }
  }

  async fn handle_send_results(
    &mut self,
    receive_inbox_result: &mut mpsc::UnboundedReceiver<SendActivityResult>,
    successfuls: &mut BinaryHeap<SendSuccessInfo>,
    in_flight: &mut i64,
  ) -> Result<(), anyhow::Error> {
    let mut force_write = false;
    let mut events = Vec::new();
    // wait for at least one event but if there's multiple handle them all
    receive_inbox_result.recv_many(&mut events, 1000).await;
    for event in events {
      match event {
        SendActivityResult::Success(s) => {
          self.state.fail_count = 0;
          *in_flight -= 1;
          if !s.was_skipped {
            self.mark_instance_alive().await?;
          }
          successfuls.push(s);
        }
        SendActivityResult::Failure { fail_count, .. } => {
          if fail_count > self.state.fail_count {
            // override fail count - if multiple activities are currently sending this value may get
            // conflicting info but that's fine
            self.state.fail_count = fail_count;
            self.state.last_retry = Some(Utc::now());
            force_write = true;
          }
        }
      }
    }
    self
      .pop_successfuls_and_write(successfuls, force_write)
      .await?;
    Ok(())
  }
  async fn mark_instance_alive(&mut self) -> Result<()> {
    // Activity send successful, mark instance as alive if it hasn't been updated in a while.
    let updated = self.instance.updated.unwrap_or(self.instance.published);
    if updated.add(Days::new(1)) < Utc::now() {
      self.instance.updated = Some(Utc::now());

      let form = InstanceForm::builder()
        .domain(self.instance.domain.clone())
        .updated(Some(naive_now()))
        .build();
      Instance::update(&mut self.pool(), self.instance.id, form).await?;
    }
    Ok(())
  }
  /// Checks that sequential activities `last_successful_id + 1`, `last_successful_id + 2` etc have
  /// been sent successfully. In that case updates `last_successful_id` and saves the state to the
  /// database if the time since the last save is greater than `SAVE_STATE_EVERY_TIME`.
  async fn pop_successfuls_and_write(
    &mut self,
    successfuls: &mut BinaryHeap<SendSuccessInfo>,
    force_write: bool,
  ) -> Result<()> {
    let Some(mut last_id) = self.state.last_successful_id else {
      tracing::warn!(
        "{} should be impossible: last successful id is None",
        self.instance.domain
      );
      return Ok(());
    };
    tracing::debug!(
      "{} last: {:?}, next: {:?}, currently in successfuls: {:?}",
      self.instance.domain,
      last_id,
      successfuls.peek(),
      successfuls.iter()
    );
    while successfuls
      .peek()
      .map(|a| a.activity_id == ActivityId(last_id.0 + 1))
      .unwrap_or(false)
    {
      let next = successfuls
        .pop()
        .context("peek above ensures pop has value")?;
      last_id = next.activity_id;
      self.state.last_successful_id = Some(next.activity_id);
      self.state.last_successful_published_time = next.published;
    }

    let save_state_every = chrono::Duration::from_std(SAVE_STATE_EVERY_TIME).expect("not negative");
    if force_write || (Utc::now() - self.last_state_insert) > save_state_every {
      self.save_and_send_state().await?;
    }
    Ok(())
  }

  /// we collect the relevant inboxes in the main instance worker task, and only spawn the send task
  /// if we have inboxes to send to this limits CPU usage and reduces overhead for the (many)
  /// cases where we don't have any inboxes
  async fn spawn_send_if_needed(
    &mut self,
    activity_id: ActivityId,
    report: UnboundedSender<SendActivityResult>,
  ) -> Result<()> {
    let Some(ele) = get_activity_cached(&mut self.pool(), activity_id)
      .await
      .context("failed reading activity from db")?
    else {
      tracing::debug!("{}: {:?} does not exist", self.instance.domain, activity_id);
      report.send(SendActivityResult::Success(SendSuccessInfo {
        activity_id,
        published: None,
        was_skipped: true,
      }))?;
      return Ok(());
    };
    let activity = &ele.0;
    let inbox_urls = self
      .inbox_collector
      .get_inbox_urls(activity)
      .await
      .context("failed figuring out inbox urls")?;
    if inbox_urls.is_empty() {
      // this is the case when the activity is not relevant to this receiving instance (e.g. no user
      // subscribed to the relevant community)
      tracing::debug!("{}: {:?} no inboxes", self.instance.domain, activity.id);
      report.send(SendActivityResult::Success(SendSuccessInfo {
        activity_id,
        // it would be valid here to either return None or Some(activity.published). The published
        // time is only used for stats pages that track federation delay. None can be a bit
        // misleading because if you look at / chart the published time for federation from a large
        // to a small instance that's only subscribed to a few small communities, then it will show
        // the last published time as a days ago even though federation is up to date.
        published: Some(activity.published),
        was_skipped: true,
      }))?;
      return Ok(());
    }
    let initial_fail_count = self.state.fail_count;
    let data = self.federation_lib_config.to_request_data();
    let stop = self.stop.clone();
    let domain = self.instance.domain.clone();
    tokio::spawn(async move {
      let mut report = report;
      let res = SendRetryTask {
        activity: &ele.0,
        object: &ele.1,
        inbox_urls,
        report: &mut report,
        initial_fail_count,
        domain,
        context: data,
        stop,
      }
      .send_retry_loop()
      .await;
      if let Err(e) = res {
        tracing::warn!(
          "sending {} errored internally, skipping activity: {:?}",
          ele.0.ap_id,
          e
        );
        report
          .send(SendActivityResult::Success(SendSuccessInfo {
            activity_id,
            published: None,
            was_skipped: true,
          }))
          .ok();
      }
    });
    Ok(())
  }

  async fn save_and_send_state(&mut self) -> Result<()> {
    tracing::debug!("{}: saving and sending state", self.instance.domain);
    self.last_state_insert = Utc::now();
    FederationQueueState::upsert(&mut self.pool(), &self.state).await?;
    self.stats_sender.send(FederationQueueStateWithDomain {
      state: self.state.clone(),
      domain: self.instance.domain.clone(),
    })?;
    Ok(())
  }

  fn pool(&self) -> DbPool<'_> {
    DbPool::Pool(&self.pool)
  }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[allow(clippy::indexing_slicing)]
mod test {

  use super::*;
  use activitypub_federation::{
    http_signatures::generate_actor_keypair,
    protocol::context::WithContext,
  };
  use actix_web::{dev::ServerHandle, web, App, HttpResponse, HttpServer};
  use lemmy_api_common::utils::{generate_inbox_url, generate_shared_inbox_url};
  use lemmy_db_schema::{
    newtypes::DbUrl,
    source::{
      activity::{ActorType, SentActivity, SentActivityForm},
      person::{Person, PersonInsertForm},
    },
    traits::Crud,
  };
  use lemmy_utils::error::LemmyResult;
  use reqwest::StatusCode;
  use serde_json::Value;
  use serial_test::serial;
  use std::{fs::File, io::BufReader};
  use test_context::{test_context, AsyncTestContext};
  use tokio::{
    spawn,
    sync::mpsc::{error::TryRecvError, unbounded_channel, UnboundedReceiver},
  };
  use tracing_test::traced_test;
  use url::Url;

  struct Data {
    context: activitypub_federation::config::Data<LemmyContext>,
    instance: Instance,
    person: Person,
    stats_receiver: UnboundedReceiver<FederationQueueStateWithDomain>,
    inbox_receiver: UnboundedReceiver<String>,
    cancel: CancellationToken,
    cleaned_up: bool,
    wait_stop_server: ServerHandle,
  }

  impl Data {
    async fn init() -> LemmyResult<Self> {
      let context = LemmyContext::init_test_federation_config().await;
      let instance = Instance::read_or_create(&mut context.pool(), "localhost".to_string()).await?;

      let actor_keypair = generate_actor_keypair()?;
      let actor_id: DbUrl = Url::parse("http://local.com/u/alice")?.into();
      let person_form = PersonInsertForm {
        actor_id: Some(actor_id.clone()),
        private_key: (Some(actor_keypair.private_key)),
        inbox_url: Some(generate_inbox_url(&actor_id)?),
        shared_inbox_url: Some(generate_shared_inbox_url(context.settings())?),
        ..PersonInsertForm::new("alice".to_string(), actor_keypair.public_key, instance.id)
      };
      let person = Person::create(&mut context.pool(), &person_form).await?;

      let cancel = CancellationToken::new();
      let (stats_sender, stats_receiver) = unbounded_channel();
      let (inbox_sender, inbox_receiver) = unbounded_channel();

      // listen for received activities in background
      let wait_stop_server = listen_activities(inbox_sender)?;

      let concurrent_sends_per_instance = std::env::var("LEMMY_TEST_FEDERATION_CONCURRENT_SENDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

      let fed_config = FederationWorkerConfig {
        concurrent_sends_per_instance
      };
      spawn(InstanceWorker::init_and_loop(
        instance.clone(),
        context.clone(),
        fed_config,
        cancel.clone(),
        stats_sender,
      ));
      // wait for startup
      sleep(*WORK_FINISHED_RECHECK_DELAY).await;

      Ok(Self {
        context: context.to_request_data(),
        instance,
        person,
        stats_receiver,
        inbox_receiver,
        cancel,
        wait_stop_server,
        cleaned_up: false,
      })
    }

    async fn cleanup(&mut self) -> LemmyResult<()> {
      if self.cleaned_up {
        return Ok(());
      }
      self.cleaned_up = true;
      self.cancel.cancel();
      sleep(*WORK_FINISHED_RECHECK_DELAY).await;
      Instance::delete_all(&mut self.context.pool()).await?;
      Person::delete(&mut self.context.pool(), self.person.id).await?;
      self.wait_stop_server.stop(true).await;
      Ok(())
    }
  }

  /// In order to guarantee that the webserver is stopped via the cleanup function,
  /// we implement a test context.
  impl AsyncTestContext for Data {
    async fn setup() -> Data {
      Data::init().await.unwrap()
    }
    async fn teardown(mut self) {
      self.cleanup().await.unwrap()
    }
  }

  #[test_context(Data)]
  #[tokio::test]
  #[traced_test]
  #[serial]
  async fn test_stats(data: &mut Data) -> LemmyResult<()> {
    tracing::debug!("hello world");

    // first receive at startup
    let rcv = data.stats_receiver.recv().await.unwrap();
    tracing::debug!("received first stats");
    assert_eq!(data.instance.id, rcv.state.instance_id);
    // assert_eq!(Some(ActivityId(0)), rcv.state.last_successful_id);
    // let last_id_before = rcv.state.last_successful_id.unwrap();

    let sent = send_activity(data.person.actor_id.clone(), &data.context, true).await?;
    tracing::debug!("sent activity");
    // receive for successfully sent activity
    let inbox_rcv = data.inbox_receiver.recv().await.unwrap();
    let parsed_activity = serde_json::from_str::<WithContext<Value>>(&inbox_rcv)?;
    assert_eq!(&sent.data, parsed_activity.inner());
    tracing::debug!("received activity");

    let rcv = data.stats_receiver.recv().await.unwrap();
    assert_eq!(data.instance.id, rcv.state.instance_id);
    assert_eq!(Some(sent.id), rcv.state.last_successful_id);

    data.cleanup().await?;

    // it also sends state on shutdown
    let rcv = data.stats_receiver.try_recv();
    assert!(rcv.is_ok());

    // nothing further received
    let rcv = data.stats_receiver.try_recv();
    assert_eq!(Some(TryRecvError::Disconnected), rcv.err());
    let inbox_rcv = data.inbox_receiver.try_recv();
    assert_eq!(Some(TryRecvError::Disconnected), inbox_rcv.err());

    Ok(())
  }

  #[test_context(Data)]
  #[tokio::test]
  #[traced_test]
  #[serial]
  async fn test_send_100(data: &mut Data) -> LemmyResult<()> {
    tracing::debug!("hello world");

    // first receive at startup
    let rcv = data.stats_receiver.recv().await.unwrap();
    tracing::debug!("received first stats");
    assert_eq!(data.instance.id, rcv.state.instance_id);
    // assert_eq!(Some(ActivityId(0)), rcv.state.last_successful_id);
    // let last_id_before = rcv.state.last_successful_id.unwrap();
    let mut sent = Vec::new();
    for _ in 0..100 {
      sent.push(send_activity(data.person.actor_id.clone(), &data.context, false).await?);
    }
    sleep(2 * *WORK_FINISHED_RECHECK_DELAY).await;
    tracing::debug!("sent activity");
    // receive for successfully sent activity
    for i in 0..100 {
      let inbox_rcv = data.inbox_receiver.recv().await.unwrap();
      let parsed_activity = serde_json::from_str::<WithContext<Value>>(&inbox_rcv)?;
      assert_eq!(&sent[i].data, parsed_activity.inner());
      tracing::debug!("received activity");
    }

    Ok(())
  }

  #[test_context(Data)]
  #[tokio::test]
  #[serial]
  async fn test_update_instance(data: &mut Data) -> LemmyResult<()> {
    let form = InstanceForm::builder()
      .domain(data.instance.domain.clone())
      .updated(None)
      .build();
    Instance::update(&mut data.context.pool(), data.instance.id, form).await?;

    send_activity(data.person.actor_id.clone(), &data.context, true).await?;
    data.inbox_receiver.recv().await.unwrap();

    let instance =
      Instance::read_or_create(&mut data.context.pool(), data.instance.domain.clone()).await?;

    assert!(instance.updated.is_some());

    data.cleanup().await?;

    Ok(())
  }

  fn listen_activities(inbox_sender: UnboundedSender<String>) -> LemmyResult<ServerHandle> {
    let run = HttpServer::new(move || {
      App::new()
        .app_data(actix_web::web::Data::new(inbox_sender.clone()))
        .route(
          "/inbox",
          web::post().to(
            |inbox_sender: actix_web::web::Data<UnboundedSender<String>>, body: String| async move {
              tracing::debug!("received activity: {:?}", body);
              inbox_sender.send(body.clone()).unwrap();
              HttpResponse::new(StatusCode::OK)
            },
          ),
        )
    })
    .bind(("127.0.0.1", 8085))?
    .run();
    let handle = run.handle();
    tokio::spawn(async move {
      run.await.unwrap();
      /*select! {
        _ = run => {},
        _ = cancel.cancelled() => { }
      }*/
    });
    Ok(handle)
  }

  async fn send_activity(
    actor_id: DbUrl,
    context: &LemmyContext,
    wait: bool,
  ) -> LemmyResult<SentActivity> {
    // create outgoing activity
    let file = File::open("../apub/assets/lemmy/activities/voting/like_note.json")?;
    let reader = BufReader::new(file);
    let form = SentActivityForm {
      ap_id: Url::parse(&format!(
        "http://local.com/activity/{}",
        uuid::Uuid::new_v4()
      ))?
      .into(),
      data: serde_json::from_reader(reader)?,
      sensitive: false,
      send_inboxes: vec![Some(Url::parse("http://localhost:8085/inbox")?.into())],
      send_all_instances: false,
      send_community_followers_of: None,
      actor_type: ActorType::Person,
      actor_apub_id: actor_id,
    };
    let sent = SentActivity::create(&mut context.pool(), form).await?;

    if wait {
      sleep(*WORK_FINISHED_RECHECK_DELAY * 2).await;
    }

    Ok(sent)
  }
}

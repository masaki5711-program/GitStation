//! The notification lane of `forge worker` (M3 N1, ADR-0016 Amendment N1):
//! the `notify` subscriber (it joins the others in the subscribers task) and
//! the `notification.send` job on **its own job runner**, like the
//! maintenance lane. A relay that answers slowly holds a send for up to
//! `notify.send_timeout_ms`; on the main runner that would delay merge and
//! provisioning jobs on this worker (the queue runs one job at a time), so
//! mail never shares a runner with them. The main runner claims only the
//! kinds it has a handler for, so it never takes a send job.
//!
//! Present only when `notify.enabled` is `true` (the keys are validated at
//! load; the relay's CA file and password are read here, once, and a bad one
//! stops the worker's start with `FG-SYS-0002`). Nothing in the lane reaches
//! the network except the send handler, and a failed mail never fails
//! anything else: its state is the notification's own.

use std::sync::Arc;

use forge_core::config::NotifyConfig;
use forge_db::jobs::{JobRunner, WorkerId};
use forge_db::Db;
use forge_errors::ForgeError;
use forge_notify::{Notify, SendHandler, Sender, SmtpSender};

/// The task's name in the supervisor's logs.
pub(crate) const RUNNER_NAME: &str = "notify_runner";

/// What the lane is built from: `notify.*` and the origin of
/// `web.base_url` (the link in a mail; none without it).
#[derive(Debug, Clone)]
pub(crate) struct NotifyLane {
    pub(crate) cfg: NotifyConfig,
    pub(crate) origin: Option<String>,
}

/// The lane's runner and subscriber.
#[derive(Clone)]
pub(crate) struct NotifyTask {
    pub(crate) runner: Arc<JobRunner>,
    pub(crate) subscriber: Arc<Notify>,
}

impl NotifyTask {
    /// `sender`: a stand-in for the relay (tests); `None` builds the SMTP
    /// sender from the configuration.
    pub(crate) fn new(
        db: &Db,
        lane: &NotifyLane,
        sender: Option<Arc<dyn Sender>>,
    ) -> Result<Self, ForgeError> {
        let sender: Arc<dyn Sender> = match sender {
            Some(sender) => sender,
            None => Arc::new(SmtpSender::new(&lane.cfg)?),
        };
        let mut runner = JobRunner::new(db.clone(), WorkerId::generate()); // clone: pool handles
        runner.register(Arc::new(SendHandler::new(
            db,
            sender,
            &lane.cfg,
            lane.origin.as_deref(),
        )?))?;
        Ok(Self {
            runner: Arc::new(runner),
            subscriber: Arc::new(Notify::new(db, &lane.cfg)?),
        })
    }
}

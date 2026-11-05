//! forge-notify: e-mail notifications (M3 N1; plan 11 §5 N1 and §10 Q6;
//! ADR-0016 Amendment N1). A feature crate: forge-web / forge-cli →
//! forge-notify → forge-auth / forge-db → forge-core → forge-errors; it
//! runs no SQL of its own (the SQL is `forge_db::notify`) and depends on no
//! other feature crate — it reads only domain events (the outbox).
//!
//! - [`Notify`] is the `notify` subscriber: merge request events become one
//!   notification row per recipient (`merge_request.approved` / `.merged` /
//!   `.commented` tell the author), decided by [`access::decide`]: the
//!   recipient's settings, address and — through `forge_auth::prove` — their
//!   right to read the merge request.
//! - [`SendHandler`] is the `notification.send` job: it decides again, builds
//!   the mail in the recipient's language ([`templates`]) and hands it to a
//!   [`Sender`] ([`SmtpSender`] in production) with the connect and send
//!   timeouts of `notify.*`. Retries and backoff are the job queue's; the
//!   outcome is the notification's own state, and a message that may already
//!   have been handed over is never sent again.
//!
//! Nothing here decides who may see what: [`access`] only asks
//! `forge_auth`. A mail never carries more than the recipient could read in
//! GitStation, and never anything internal ([`templates`]).

pub mod access;
pub mod job;
pub mod mail;
pub mod smtp;
pub mod subscriber;
pub mod templates;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use job::SendHandler;
pub use mail::Mail;
pub use smtp::{Reason, SendFailure, SendOutcome, Sender, SmtpSender, Stage};
pub use subscriber::{Notify, NAME as SUBSCRIBER_NAME};

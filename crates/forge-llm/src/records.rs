//! Where the gateway writes its records: the database
//! ([`DbRecords`], `forge_db::llm`) or a stand-in in tests.

use std::future::Future;
use std::pin::Pin;

use forge_db::llm::{Ending, NewAdvice, NewRequest, Usage};
use forge_db::Db;
use forge_errors::ForgeError;

/// The boxed future of a [`Records`] call.
pub type RecordFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ForgeError>> + Send + 'a>>;

/// The gateway's writes.
pub trait Records: Send + Sync {
    /// A request was queued; its id.
    fn queued<'a>(&'a self, r: &'a NewRequest) -> RecordFuture<'a, i64>;
    /// Its turn came.
    fn running(&self, id: i64) -> RecordFuture<'_, bool>;
    /// It ended.
    fn finished(&self, id: i64, ending: Ending, usage: Usage) -> RecordFuture<'_, bool>;
    /// Its advice; the advice's id.
    fn advice<'a>(&'a self, a: &'a NewAdvice) -> RecordFuture<'a, i64>;
}

/// The records in `forge.llm_requests` / `forge.llm_advice`.
#[derive(Debug, Clone)]
pub struct DbRecords(pub Db);

impl Records for DbRecords {
    fn queued<'a>(&'a self, r: &'a NewRequest) -> RecordFuture<'a, i64> {
        Box::pin(forge_db::llm::insert_request(&self.0, r))
    }

    fn running(&self, id: i64) -> RecordFuture<'_, bool> {
        Box::pin(forge_db::llm::mark_running(&self.0, id))
    }

    fn finished(&self, id: i64, ending: Ending, usage: Usage) -> RecordFuture<'_, bool> {
        Box::pin(forge_db::llm::finish(&self.0, id, ending, usage))
    }

    fn advice<'a>(&'a self, a: &'a NewAdvice) -> RecordFuture<'a, i64> {
        Box::pin(forge_db::llm::insert_advice(&self.0, a))
    }
}

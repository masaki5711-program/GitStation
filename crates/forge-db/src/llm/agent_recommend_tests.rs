//! The recommendation's LLM asks against a migrated database (plan 16 A11).

use forge_core::llm::agent_recommend::{reasons_state, ReasonsState};
use forge_core::RequestId;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::projects::{self, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

async fn world() -> (FreshDb, ProjectId) {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &system("req-user-alice"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let web = projects::create(
        &t.db,
        &system("req-project-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web");
    (t, web.project_id)
}

const TIP_A: &str = "1111111111111111111111111111111111111111";
const TIP_B: &str = "2222222222222222222222222222222222222222";

#[tokio::test]
async fn an_ask_is_pending_until_answered_for_its_tip() {
    let (t, project) = world().await;
    let first = upsert_ask(&t.db, project, Some(TIP_A), "req-1")
        .await
        .expect("ask");
    assert_eq!(first.state, AskState::Pending);
    assert_eq!(
        reasons_state(Some(&first), Some(TIP_A)),
        ReasonsState::Pending
    );
    let asks = recent_asks(&t.db, 3600, 600, 10).await.expect("asks");
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].project, project);
    assert_eq!(asks[0].request_id, "req-1");

    // The worker could not reach the LLM: the reader is told so.
    assert!(record_answer(
        &t.db,
        project,
        &Answer {
            state: AskState::Unavailable,
            head_sha: Some(TIP_A),
            error_code: Some(ErrorCode::LlmUnavailable),
            advice_id: None,
        },
    )
    .await
    .expect("answer"));
    let again = upsert_ask(&t.db, project, Some(TIP_A), "req-2")
        .await
        .expect("ask");
    assert_eq!(again.state, AskState::Unavailable);
    assert_eq!(again.error_code.as_deref(), Some("FG-LLM-0001"));
    assert_eq!(
        reasons_state(Some(&again), Some(TIP_A)),
        ReasonsState::Unavailable
    );

    // A new tip asks again.
    let moved = upsert_ask(&t.db, project, Some(TIP_B), "req-3")
        .await
        .expect("ask");
    assert_eq!(moved.state, AskState::Pending);
    assert_eq!(moved.error_code, None);
    assert!(moved.pending_secs < 60);
    t.drop_now().await;
}

#[tokio::test]
async fn a_bad_answer_is_refused_by_the_table() {
    let (t, project) = world().await;
    upsert_ask(&t.db, project, None, "req-1")
        .await
        .expect("ask");
    // `ready` needs the tip it is for.
    let refused = record_answer(
        &t.db,
        project,
        &Answer {
            state: AskState::Ready,
            head_sha: None,
            error_code: None,
            advice_id: None,
        },
    )
    .await;
    assert!(refused.is_err());
    // A purged project's ask is gone: nothing to answer.
    let other = ProjectId::new(project.get() + 1000);
    assert!(!record_answer(
        &t.db,
        other,
        &Answer {
            state: AskState::Paused,
            head_sha: None,
            error_code: None,
            advice_id: None,
        },
    )
    .await
    .expect("no row"));
    t.drop_now().await;
}

#[tokio::test]
async fn answered_asks_never_take_the_place_of_waiting_ones() {
    let (t, project) = world().await;
    upsert_ask(&t.db, project, Some(TIP_A), "req-1")
        .await
        .expect("ask");
    for state in [AskState::Unavailable, AskState::Refused] {
        assert!(record_answer(
            &t.db,
            project,
            &Answer {
                state,
                head_sha: Some(TIP_A),
                error_code: None,
                advice_id: None,
            },
        )
        .await
        .expect("answer"));
        // Within the retry wait: not handed out.
        assert!(recent_asks(&t.db, 3600, 600, 10)
            .await
            .expect("asks")
            .is_empty());
        // Re-reading does not postpone the retry: past it, handed out.
        upsert_ask(&t.db, project, Some(TIP_A), "req-again")
            .await
            .expect("ask");
        assert_eq!(
            recent_asks(&t.db, 3600, 0, 10).await.expect("asks").len(),
            1
        );
    }
    // Paused: asked again every pass.
    assert!(record_answer(
        &t.db,
        project,
        &Answer {
            state: AskState::Paused,
            head_sha: Some(TIP_A),
            error_code: None,
            advice_id: None,
        },
    )
    .await
    .expect("answer"));
    assert_eq!(
        recent_asks(&t.db, 3600, 600, 10).await.expect("asks").len(),
        1
    );
    t.drop_now().await;
}

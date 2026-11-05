//! The loop that carries out export plans.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use tracing::{info, warn};

use super::bridge::{Command, CommandResult, Inverter};
use super::plan::{self, Action, FinishReason, Status};
use crate::db::{self, PlanRow};

/// How long a command stays valid on its way to the inverter.
const COMMAND_TTL: Duration = Duration::seconds(30);

/// One pass: decide and act on every open plan.
pub async fn tick(
    pool: &PgPool,
    inverter: &dyn Inverter,
    now: DateTime<Utc>,
    soc: Option<f64>,
) -> anyhow::Result<()> {
    let rows = db::open_plans(pool).await?;
    let plans: Vec<_> = rows.iter().map(PlanRow::plan).collect();
    for action in plan::decide(now, &plans, soc) {
        let id = match &action {
            Action::Start(id) | Action::Finish(id, _) | Action::Close(id, _, _) => *id,
        };
        let Some(row) = rows.iter().find(|r| r.id == id) else {
            continue;
        };
        let ctx = Ctx {
            pool,
            inverter,
            now,
            plan: row,
        };
        match action {
            Action::Start(_) => ctx.start(soc).await?,
            Action::Finish(_, reason) => ctx.finish(reason).await?,
            Action::Close(_, status, note) => ctx.close(status, note).await?,
        }
    }
    Ok(())
}

struct Ctx<'a> {
    pool: &'a PgPool,
    inverter: &'a dyn Inverter,
    now: DateTime<Utc>,
    plan: &'a PlanRow,
}

impl Ctx<'_> {
    /// Sends a command and logs it with its answer. `Err` means no answer.
    async fn send(
        &self,
        purpose: &str,
        write: Vec<(String, f64)>,
        read: Vec<String>,
    ) -> anyhow::Result<Result<CommandResult, String>> {
        let cmd = Command {
            id: format!(
                "plan{}-{purpose}-{}",
                self.plan.id,
                self.now.timestamp_millis()
            ),
            expires: (self.now + COMMAND_TTL).timestamp().max(0) as u64,
            write,
            read,
        };
        db::log_command(
            self.pool,
            &cmd.id,
            Some(self.plan.id),
            purpose,
            &serde_json::to_value(&cmd)?,
        )
        .await?;
        let id = cmd.id.clone();
        match self.inverter.send(cmd).await {
            Ok(result) => {
                db::log_result(self.pool, &id, result.ok, &serde_json::to_value(&result)?).await?;
                Ok(Ok(result))
            }
            Err(e) => {
                let msg = format!("{e:#}");
                db::log_result(self.pool, &id, false, &serde_json::json!({ "error": msg })).await?;
                Ok(Err(msg))
            }
        }
    }

    async fn retry(&self, note: &str) -> anyhow::Result<()> {
        warn!(plan = self.plan.id, "{note}");
        db::mark_retry(self.pool, self.plan.id, self.now + plan::RETRY_AFTER, note).await?;
        Ok(())
    }

    /// Reads the settings to restore (once), then applies the export.
    async fn start(&self, soc: Option<f64>) -> anyhow::Result<()> {
        let id = self.plan.id;
        let saved = match &self.plan.saved_settings {
            Some(saved) => saved.0.clone(),
            None => match self
                .send("baseline", Vec::new(), plan::export_keys())
                .await?
            {
                Ok(r) if r.ok => {
                    db::save_baseline(self.pool, id, &r.before, soc).await?;
                    r.before
                }
                Ok(r) => {
                    let err = r.error.unwrap_or_default();
                    return self
                        .retry(&format!("reading inverter settings failed: {err}"))
                        .await;
                }
                Err(e) => {
                    return self
                        .retry(&format!("reading inverter settings failed: {e}"))
                        .await;
                }
            },
        };
        if let Some(missing) = plan::export_keys()
            .into_iter()
            .find(|k| !saved.contains_key(k))
        {
            db::mark_closed(
                self.pool,
                id,
                Status::Failed,
                &format!("the bridge did not report {missing}"),
            )
            .await?;
            return Ok(());
        }

        let writes = plan::export_writes(self.plan.target_soc, self.plan.power_w);
        match self.send("start", writes, Vec::new()).await? {
            Ok(r) if r.ok => {
                info!(plan = id, dry_run = r.dry_run, "export started");
                db::mark_active(self.pool, id, r.dry_run).await?;
            }
            Ok(r) if r.written.is_empty() => {
                let err = r.error.unwrap_or_default();
                self.retry(&format!("starting failed: {err}")).await?;
            }
            Ok(r) => {
                // Some settings changed: put everything back now.
                let err = r.error.unwrap_or_default();
                match self.restore(&saved).await? {
                    Ok(()) => {
                        db::mark_closed(
                            self.pool,
                            id,
                            Status::Failed,
                            &format!("starting failed: {err}; settings restored"),
                        )
                        .await?;
                    }
                    Err(e2) => {
                        // Treat it as running and let finish keep restoring.
                        db::mark_active(self.pool, id, false).await?;
                        db::request_cancel(self.pool, id).await?;
                        self.retry(&format!("starting failed: {err}; restoring failed: {e2}"))
                            .await?;
                    }
                }
            }
            // No answer: the export may or may not be running. Retrying is
            // safe (same values), and closing the plan always restores.
            Err(e) => self.retry(&format!("starting failed: {e}")).await?,
        }
        Ok(())
    }

    async fn restore(&self, saved: &BTreeMap<String, f64>) -> anyhow::Result<Result<(), String>> {
        Ok(
            match self
                .send("restore", plan::restore_writes(saved), Vec::new())
                .await?
            {
                Ok(r) if r.ok => Ok(()),
                Ok(r) => Err(r.error.unwrap_or_default()),
                Err(e) => Err(e),
            },
        )
    }

    async fn finish(&self, reason: FinishReason) -> anyhow::Result<()> {
        let status = match reason {
            FinishReason::Cancelled => Status::Cancelled,
            FinishReason::TargetReached | FinishReason::EndTime => Status::Done,
        };
        self.restore_and_close(status, reason.as_str()).await
    }

    /// Closes a plan that never became active. If it got as far as reading a
    /// baseline, a start may have been written without an answer, so the
    /// baseline is written back first.
    async fn close(&self, status: Status, note: &str) -> anyhow::Result<()> {
        if self.plan.saved_settings.is_some() {
            self.restore_and_close(status, note).await
        } else {
            db::mark_closed(self.pool, self.plan.id, status, note).await?;
            Ok(())
        }
    }

    async fn restore_and_close(&self, status: Status, note: &str) -> anyhow::Result<()> {
        if let Some(saved) = &self.plan.saved_settings
            && let Err(e) = self.restore(&saved.0).await?
        {
            return self.retry(&format!("restoring settings failed: {e}")).await;
        }
        info!(plan = self.plan.id, status = status.as_str(), "{note}");
        db::mark_closed(self.pool, self.plan.id, status, note).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;
    use crate::control::bridge::BoxFuture;
    use crate::db::NewPlan;

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    const START: &str = "2026-11-05T17:00:00Z";

    fn at(minutes: i64) -> DateTime<Utc> {
        utc(START) + Duration::minutes(minutes)
    }

    enum Failure {
        /// No answer; the command was not carried out.
        Lost,
        /// No answer, but the command was carried out.
        LostAfterWriting,
        /// The bridge refused it; nothing written.
        Refused,
        /// The first `n` writes went through, then an error.
        Partial(usize),
    }

    /// An inverter with the export settings, answering like the bridge.
    struct FakeInverter {
        state: Mutex<BTreeMap<String, f64>>,
        sent: Mutex<Vec<Command>>,
        failures: Mutex<VecDeque<Option<Failure>>>,
        dry_run: bool,
    }

    fn normal_settings() -> BTreeMap<String, f64> {
        plan::export_keys()
            .into_iter()
            .map(|k| {
                let v = match k.as_str() {
                    "work_mode" => 2.0,
                    "tou_selling" => 1.0,
                    "solar_sell" => 0.0,
                    k if k.starts_with("tou_soc") => 15.0,
                    k if k.contains("power") => 4000.0,
                    _ => 1.0,
                };
                (k, v)
            })
            .collect()
    }

    impl FakeInverter {
        fn new() -> Self {
            Self {
                state: Mutex::new(normal_settings()),
                sent: Mutex::default(),
                failures: Mutex::default(),
                dry_run: false,
            }
        }

        /// Scripts the next commands: `None` answers normally.
        fn then(&self, f: Option<Failure>) -> &Self {
            self.failures.lock().unwrap().push_back(f);
            self
        }

        fn sent(&self) -> Vec<Command> {
            self.sent.lock().unwrap().clone()
        }

        fn state(&self) -> BTreeMap<String, f64> {
            self.state.lock().unwrap().clone()
        }
    }

    impl Inverter for FakeInverter {
        fn send(&self, cmd: Command) -> BoxFuture<'_, anyhow::Result<CommandResult>> {
            Box::pin(async move {
                self.sent.lock().unwrap().push(cmd.clone());
                let failure = self.failures.lock().unwrap().pop_front().flatten();
                let mut state = self.state.lock().unwrap();
                let keys: Vec<&String> =
                    cmd.write.iter().map(|(k, _)| k).chain(&cmd.read).collect();
                let snapshot = |s: &BTreeMap<String, f64>| {
                    keys.iter()
                        .map(|k| ((*k).clone(), s[*k]))
                        .collect::<BTreeMap<_, _>>()
                };
                let before = snapshot(&state);
                let mut result = CommandResult {
                    id: cmd.id.clone(),
                    ok: true,
                    dry_run: self.dry_run,
                    before,
                    ..CommandResult::default()
                };
                let limit = match failure {
                    Some(Failure::Lost) => anyhow::bail!("no answer from the bridge"),
                    Some(Failure::Refused) => 0,
                    Some(Failure::Partial(n)) => n,
                    Some(Failure::LostAfterWriting) | None => usize::MAX,
                };
                if !self.dry_run {
                    for (k, v) in cmd.write.iter().take(limit) {
                        state.insert(k.clone(), *v);
                        result.written.push(k.clone());
                    }
                }
                result.after = snapshot(&state);
                match failure {
                    Some(Failure::LostAfterWriting) => anyhow::bail!("no answer from the bridge"),
                    Some(Failure::Refused | Failure::Partial(_)) => {
                        result.ok = false;
                        result.error = Some("exception 4 (slave device failure)".into());
                    }
                    _ => {}
                }
                Ok(result)
            })
        }
    }

    async fn scheduled(pool: &PgPool) -> i64 {
        db::insert_plan(
            pool,
            &NewPlan {
                start: at(0),
                end: at(240),
                target_soc: 30,
                power_w: 3500,
                projected_kwh: None,
                projected_pln: None,
            },
        )
        .await
        .unwrap()
    }

    async fn row(pool: &PgPool, id: i64) -> PlanRow {
        db::plan(pool, id).await.unwrap().unwrap()
    }

    fn exporting() -> BTreeMap<String, f64> {
        let mut s = normal_settings();
        for (k, v) in plan::export_writes(30, 3500) {
            s.insert(k, v);
        }
        s
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn starts_with_a_baseline_then_restores_it(pool: PgPool) {
        let inv = FakeInverter::new();
        let id = scheduled(&pool).await;

        tick(&pool, &inv, at(-1), Some(80.0)).await.unwrap();
        assert!(inv.sent().is_empty(), "not due yet");

        tick(&pool, &inv, at(0), Some(80.0)).await.unwrap();
        let sent = inv.sent();
        assert_eq!(sent.len(), 2);
        assert!(sent[0].write.is_empty(), "first a read-only baseline");
        assert_eq!(sent[0].read, plan::export_keys());
        assert_eq!(sent[1].write, plan::export_writes(30, 3500));
        assert!(sent[1].expires > at(0).timestamp() as u64);
        let r = row(&pool, id).await;
        assert_eq!(r.status(), Status::Active);
        assert_eq!(r.dry_run, Some(false));
        assert_eq!(r.soc_at_start, Some(80.0));
        assert_eq!(r.saved_settings.unwrap().0, normal_settings());
        assert_eq!(inv.state(), exporting());

        tick(&pool, &inv, at(30), Some(55.0)).await.unwrap();
        assert_eq!(inv.sent().len(), 2, "still exporting");

        tick(&pool, &inv, at(60), Some(30.0)).await.unwrap();
        let sent = inv.sent();
        assert_eq!(sent.len(), 3);
        assert_eq!(
            sent[2].write[0].0, "work_mode",
            "restore stops selling first"
        );
        assert_eq!(inv.state(), normal_settings());
        let r = row(&pool, id).await;
        assert_eq!(r.status(), Status::Done);
        assert!(r.note.unwrap().contains("target"));

        let logged: i64 =
            sqlx::query_scalar("select count(*) from inverter_command where plan_id = $1 and ok")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(logged, 3, "every command is logged with its answer");
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn refused_start_is_retried_later(pool: PgPool) {
        let inv = FakeInverter::new();
        inv.then(None).then(Some(Failure::Refused));
        let id = scheduled(&pool).await;

        tick(&pool, &inv, at(0), Some(80.0)).await.unwrap();
        let r = row(&pool, id).await;
        assert_eq!(r.status(), Status::Scheduled);
        assert!(r.note.unwrap().contains("exception 4"));
        assert_eq!(r.retry_after, Some(at(0) + plan::RETRY_AFTER));

        tick(&pool, &inv, at(0) + Duration::seconds(30), Some(80.0))
            .await
            .unwrap();
        assert_eq!(inv.sent().len(), 2, "waits before retrying");

        tick(&pool, &inv, at(1), Some(80.0)).await.unwrap();
        let sent = inv.sent();
        assert_eq!(sent.len(), 3, "baseline is not read again");
        assert_eq!(row(&pool, id).await.status(), Status::Active);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn unreadable_inverter_is_retried(pool: PgPool) {
        let inv = FakeInverter::new();
        inv.then(Some(Failure::Lost));
        let id = scheduled(&pool).await;
        tick(&pool, &inv, at(0), Some(80.0)).await.unwrap();
        let r = row(&pool, id).await;
        assert_eq!(r.status(), Status::Scheduled);
        assert!(r.saved_settings.is_none());
        assert!(r.note.unwrap().contains("no answer"));
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn partial_start_is_rolled_back(pool: PgPool) {
        let inv = FakeInverter::new();
        inv.then(None).then(Some(Failure::Partial(5)));
        let id = scheduled(&pool).await;
        tick(&pool, &inv, at(0), Some(80.0)).await.unwrap();
        assert_eq!(inv.sent().len(), 3, "baseline, start, restore");
        assert_eq!(inv.state(), normal_settings());
        let r = row(&pool, id).await;
        assert_eq!(r.status(), Status::Failed);
        assert!(r.note.unwrap().contains("restored"));
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn unanswered_start_is_restored_when_the_plan_closes(pool: PgPool) {
        let inv = FakeInverter::new();
        // The export settings were written, but the answer was lost; every
        // retry is lost too.
        inv.then(None).then(Some(Failure::LostAfterWriting));
        for _ in 0..20 {
            inv.then(Some(Failure::LostAfterWriting));
        }
        let id = scheduled(&pool).await;
        tick(&pool, &inv, at(0), Some(80.0)).await.unwrap();
        assert_eq!(row(&pool, id).await.status(), Status::Scheduled);
        assert_eq!(inv.state(), exporting());

        // The answers come back by the time the plan has run out.
        inv.failures.lock().unwrap().clear();
        tick(&pool, &inv, at(240), Some(60.0)).await.unwrap();
        assert_eq!(inv.state(), normal_settings(), "restored before closing");
        assert_eq!(row(&pool, id).await.status(), Status::Missed);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn cancelling_an_active_export_restores(pool: PgPool) {
        let inv = FakeInverter::new();
        let id = scheduled(&pool).await;
        tick(&pool, &inv, at(0), Some(80.0)).await.unwrap();
        db::request_cancel(&pool, id).await.unwrap();
        tick(&pool, &inv, at(5), Some(78.0)).await.unwrap();
        assert_eq!(inv.state(), normal_settings());
        assert_eq!(row(&pool, id).await.status(), Status::Cancelled);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn failed_restore_is_retried(pool: PgPool) {
        let inv = FakeInverter::new();
        let id = scheduled(&pool).await;
        tick(&pool, &inv, at(0), Some(80.0)).await.unwrap();
        inv.then(Some(Failure::Lost));
        tick(&pool, &inv, at(240), Some(50.0)).await.unwrap();
        let r = row(&pool, id).await;
        assert_eq!(r.status(), Status::Active, "still needs restoring");
        assert!(r.note.unwrap().contains("restor"));

        tick(&pool, &inv, at(241), Some(50.0)).await.unwrap();
        assert_eq!(inv.state(), normal_settings());
        assert_eq!(row(&pool, id).await.status(), Status::Done);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn cancelling_before_the_start_sends_nothing(pool: PgPool) {
        let inv = FakeInverter::new();
        let id = scheduled(&pool).await;
        db::request_cancel(&pool, id).await.unwrap();
        tick(&pool, &inv, at(-10), Some(80.0)).await.unwrap();
        assert!(inv.sent().is_empty());
        assert_eq!(row(&pool, id).await.status(), Status::Cancelled);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn dry_run_is_recorded(pool: PgPool) {
        let mut inv = FakeInverter::new();
        inv.dry_run = true;
        let id = scheduled(&pool).await;
        tick(&pool, &inv, at(0), Some(80.0)).await.unwrap();
        let r = row(&pool, id).await;
        assert_eq!(r.status(), Status::Active);
        assert_eq!(r.dry_run, Some(true));
        assert_eq!(inv.state(), normal_settings());
    }
}

//! Port of `apps/api/app/services/tasks.py`: long-running work is a `TaskRecord`; HTTP returns
//! 202 plus a task reference.
//!
//! `run` executes inline: QUEUED -> RUNNING (attempt + 1, `task.started`), then the work inside a
//! savepoint; on failure the work's writes are rolled back and the task is FAILED
//! (`task.failed`, `error_code` = the Python exception class name), otherwise COMPLETED with its
//! output (`task.completed`). Event idempotency keys are `task:{id}:{status}:{attempt}`.
//!
//! For work that borrows from its caller, use [`TaskService::start`] and
//! [`TaskService::finish`] around the work instead of the closure form.

use futures_util::future::BoxFuture;
use serde_json::json;
use uuid::Uuid;

use crate::app::App;
use crate::contracts::{
    ActorType, EventType, JsonMap, Subsystem, TaskRecord, TaskStatus, TaskType,
};
use crate::error::{AppError, AppResult};
use crate::logging;
use crate::services::events::Emit;
use crate::store::repo::TASKS;
use crate::store::{Savepoint, Tx};

/// A task that has been started; its work runs inside `savepoint`.
#[derive(Debug)]
pub struct RunningTask {
    pub task: TaskRecord,
    savepoint: Savepoint,
}

pub struct TaskService<'a> {
    app: &'a App,
}

/// `str(exc)[:500]` (code points).
fn truncate_500(s: &str) -> String {
    s.chars().take(500).collect()
}

impl<'a> TaskService<'a> {
    pub fn new(app: &'a App) -> Self {
        Self { app }
    }

    pub async fn create(
        &self,
        tx: &mut dyn Tx,
        project_id: Uuid,
        task_type: TaskType,
        subsystem: Subsystem,
        input_reference: JsonMap,
    ) -> AppResult<TaskRecord> {
        let task = TaskRecord::new(project_id, task_type, subsystem, input_reference);
        TASKS.add(tx, &task).await?;
        Ok(task)
    }

    async fn save(&self, tx: &mut dyn Tx, task: &TaskRecord) -> AppResult<()> {
        TASKS.upsert(tx, task).await
    }

    async fn event(&self, tx: &mut dyn Tx, task: &TaskRecord, et: EventType) -> AppResult<()> {
        self.app
            .events()
            .emit(
                tx,
                Emit::new(
                    et,
                    task.project_id,
                    ActorType::System,
                    "task-service",
                    format!(
                        "task:{}:{}:{}",
                        task.task_id,
                        task.status.as_str(),
                        task.attempt_count
                    ),
                    json!({
                        "task_id": task.task_id.to_string(),
                        "task_type": task.task_type.as_str(),
                        "status": task.status.as_str(),
                        "error_code": task.error_code,
                    }),
                )
                .correlation(task.correlation_id),
            )
            .await?;
        Ok(())
    }

    /// First half of `run`: RUNNING, attempt + 1, `task.started`, savepoint for the work.
    pub async fn start(&self, tx: &mut dyn Tx, task: TaskRecord) -> AppResult<RunningTask> {
        let mut task = task.transition(TaskStatus::Running)?;
        task.attempt_count += 1;
        self.save(tx, &task).await?;
        self.event(tx, &task, EventType::TaskStarted).await?;
        let savepoint = tx.savepoint().await?;
        Ok(RunningTask { task, savepoint })
    }

    /// Second half of `run`: record the work's outcome.
    pub async fn finish(
        &self,
        tx: &mut dyn Tx,
        running: RunningTask,
        outcome: AppResult<JsonMap>,
    ) -> AppResult<TaskRecord> {
        let RunningTask { task, savepoint } = running;
        match outcome {
            Err(err) => {
                tx.rollback_to_savepoint(savepoint).await?;
                let message = match &err {
                    AppError::Internal(i) => {
                        logging::warning(
                            "app.services.tasks",
                            "task failed with an unexpected error",
                            &[
                                ("task_id", json!(task.task_id.to_string())),
                                ("error_kind", json!(i.kind)),
                                ("exc", json!(i.cause)),
                            ],
                        );
                        // Never store internals in a record the client can read.
                        "unexpected server error".to_owned()
                    }
                    other => other.to_string(),
                };
                let mut task = task.transition(TaskStatus::Failed)?;
                task.error_code = Some(err.python_class_name().to_owned());
                task.error_message = Some(truncate_500(&message));
                self.save(tx, &task).await?;
                self.event(tx, &task, EventType::TaskFailed).await?;
                Ok(task)
            }
            Ok(output) => {
                tx.release_savepoint(savepoint).await?;
                let mut task = task.transition(TaskStatus::Completed)?;
                task.output_reference = output;
                self.save(tx, &task).await?;
                self.event(tx, &task, EventType::TaskCompleted).await?;
                Ok(task)
            }
        }
    }

    /// FAILED with `err`, keeping what the work wrote (unlike [`TaskService::finish`], which
    /// rolls it back): for work whose records are themselves the account of why it failed.
    pub async fn fail_keeping(
        &self,
        tx: &mut dyn Tx,
        running: RunningTask,
        err: AppError,
    ) -> AppResult<TaskRecord> {
        let RunningTask { task, savepoint } = running;
        tx.release_savepoint(savepoint).await?;
        let mut task = task.transition(TaskStatus::Failed)?;
        task.error_code = Some(err.python_class_name().to_owned());
        task.error_message = Some(truncate_500(&err.to_string()));
        self.save(tx, &task).await?;
        self.event(tx, &task, EventType::TaskFailed).await?;
        Ok(task)
    }

    /// `run(task, fn)`: execute `work` inline inside a savepoint.
    pub async fn run<F>(&self, tx: &mut dyn Tx, task: TaskRecord, work: F) -> AppResult<TaskRecord>
    where
        F: for<'t> FnOnce(&'t mut dyn Tx, &'t TaskRecord) -> BoxFuture<'t, AppResult<JsonMap>>
            + Send,
    {
        let running = self.start(tx, task).await?;
        let snapshot = running.task.clone();
        let outcome = work(tx, &snapshot).await;
        self.finish(tx, running, outcome).await
    }
}

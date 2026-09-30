//! 可选的 RAG 绝对截止时间，嵌套阶段和每次上游尝试共用同一时钟。
use std::{
    future::Future,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::time::Instant;

tokio::task_local! {
    static ACTIVE: Budget;
}

#[derive(Clone, Debug)]
pub struct Budget {
    deadline: Instant,
    total_duration: Duration,
    request_id: String,
    stage: &'static str,
    cancelled: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetElapsed {
    Deadline,
    ChannelTimeout,
    Cancelled,
}

impl Budget {
    /// 客户端预算只能收紧请求，服务端始终将其限制在 100ms 到 120s。
    pub fn new(timeout_ms: u64, request_id: impl Into<String>) -> Self {
        let total_duration = Duration::from_millis(timeout_ms.clamp(100, 120_000));
        Self {
            deadline: Instant::now() + total_duration,
            total_duration,
            request_id: request_id.into(),
            stage: "request",
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn total_duration(&self) -> Duration {
        self.total_duration
    }

    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub fn stage_name(&self) -> &'static str {
        self.stage
    }

    pub fn cancellation(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn check(&self) -> Result<(), BudgetElapsed> {
        if self.cancelled.load(Ordering::Acquire) {
            Err(BudgetElapsed::Cancelled)
        } else if self.remaining().is_zero() {
            Err(BudgetElapsed::Deadline)
        } else {
            Ok(())
        }
    }

    /// 给后续阶段预留时间；嵌套阶段不能延长父阶段或总请求的截止时间。
    pub fn stage(&self, stage: &'static str, cap: Duration, reserve: Duration) -> Self {
        let now = Instant::now();
        let available = self.remaining().saturating_sub(reserve);
        let mut child = self.clone();
        child.deadline = self.deadline.min(now + available.min(cap));
        child.stage = stage;
        child
    }

    pub fn scope<F: Future>(&self, future: F) -> impl Future<Output = F::Output> {
        ACTIVE.scope(self.clone(), Box::pin(future))
    }
}

pub fn current() -> Option<Budget> {
    ACTIVE.try_with(Clone::clone).ok()
}

/// 没有显式预算的历史请求保持原有行为；有预算时覆盖发送、读正文和解码。
pub fn run<F: Future>(
    cap: Duration,
    future: F,
) -> impl Future<Output = Result<F::Output, BudgetElapsed>> {
    // 在建立外层 Future 前移到堆上，避免层层 timeout/scope 放大 Rust Future
    // 栈尺寸；保留静态类型，不引入动态分发。
    let future = Box::pin(future);
    async move {
        let Some(budget) = current() else {
            return Ok(future.await);
        };
        budget.check()?;
        let channel_deadline = Instant::now() + cap.min(Duration::from_secs(120));
        let deadline = budget.deadline.min(channel_deadline);
        let elapsed = if budget.deadline <= channel_deadline {
            BudgetElapsed::Deadline
        } else {
            BudgetElapsed::ChannelTimeout
        };
        tokio::time::timeout_at(deadline, future)
            .await
            .map_err(|_| elapsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn nested_stages_reserve_time_without_resetting_total_deadline() {
        let budget = Budget::new(1_000, "test");
        let child = budget.stage(
            "embedding",
            Duration::from_secs(5),
            Duration::from_millis(400),
        );
        assert!(child.deadline() <= budget.deadline() - Duration::from_millis(399));
        assert_eq!(child.total_duration(), Duration::from_secs(1));
        let nested = child.stage("nested", Duration::from_secs(5), Duration::ZERO);
        assert!(nested.deadline() <= child.deadline());
        budget.cancel();
        assert_eq!(nested.check(), Err(BudgetElapsed::Cancelled));
    }

    #[tokio::test]
    async fn deadline_and_channel_cap_are_distinct_and_optional() {
        let budget = Budget::new(100, "test");
        let stage = budget.stage("embedding", Duration::from_millis(10), Duration::ZERO);
        assert_eq!(
            stage
                .scope(run(Duration::from_secs(1), std::future::pending::<()>()))
                .await,
            Err(BudgetElapsed::Deadline)
        );
        assert_eq!(
            budget
                .scope(run(Duration::from_millis(5), std::future::pending::<()>()))
                .await,
            Err(BudgetElapsed::ChannelTimeout)
        );
        assert_eq!(run(Duration::ZERO, std::future::ready(7)).await, Ok(7));
        assert_eq!(
            Budget::new(0, "test").total_duration(),
            Duration::from_millis(100)
        );
        assert_eq!(
            Budget::new(u64::MAX, "test").total_duration(),
            Duration::from_secs(120)
        );
    }
}

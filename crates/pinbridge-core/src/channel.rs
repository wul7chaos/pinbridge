//! 有界事件通道。
//!
//! 为什么"有界"是硬性要求：agent 跑在**被分析进程内部**，事件消费者是脚本或
//! 上层 Hub。一旦目标进入 VMP 风格的指令级循环，事件产生速度可以是每秒千万条；
//! 无界队列会在几秒内把目标进程自己撑爆——分析工具导致了分析对象崩溃，
//! 这是不可接受的（也会污染分析结论）。
//!
//! 因此：容量固定，满了按策略处理，且**丢弃计数必须可读**（出现在轨迹 summary
//! 与 MCP tool_result 里）。任何"悄悄丢事件"的实现都视为缺陷。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// 通道满时的策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropPolicy {
    /// 丢弃最早的事件，保留最新现场。交互式分析（找"现在发生了什么"）用这个。
    DropOldest,
    /// 丢弃最新的事件，保留轨迹起点。录轨迹时用这个，保证前缀完整。
    DropNewest,
    /// 拒绝写入并返回错误。用于"宁可断链也不丢证据"的取证模式。
    Reject,
}

#[derive(Debug)]
struct Shared<T> {
    queue: Mutex<VecDeque<T>>,
    dropped: AtomicU64,
    closed: AtomicBool,
    capacity: usize,
    policy: DropPolicy,
}

/// 写入端。可克隆，多线程共享。
#[derive(Debug)]
pub struct BoundedSender<T> {
    shared: Arc<Shared<T>>,
}

/// 读取端。刻意不实现 `Clone`：一个通道只有一个消费者，
/// 保证事件顺序不会被两个消费者各自解释。
#[derive(Debug)]
pub struct BoundedReceiver<T> {
    shared: Arc<Shared<T>>,
}

/// 建一个容量为 `capacity` 的通道；`capacity == 0` 会被提升为 1。
#[must_use]
pub fn bounded<T>(capacity: usize, policy: DropPolicy) -> (BoundedSender<T>, BoundedReceiver<T>) {
    let shared = Arc::new(Shared {
        queue: Mutex::new(VecDeque::with_capacity(capacity.max(1))),
        dropped: AtomicU64::new(0),
        closed: AtomicBool::new(false),
        capacity: capacity.max(1),
        policy,
    });
    (
        BoundedSender {
            shared: Arc::clone(&shared),
        },
        BoundedReceiver { shared },
    )
}

impl<T> Clone for BoundedSender<T> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> BoundedSender<T> {
    /// 尝试写入。满了按策略处理，返回值表示"这条是否进入队列"。
    pub fn send(&self, item: T) -> bool {
        if self.shared.closed.load(Ordering::Acquire) {
            return false;
        }
        let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        if queue.len() < self.shared.capacity {
            queue.push_back(item);
            return true;
        }
        match self.shared.policy {
            DropPolicy::DropNewest | DropPolicy::Reject => {
                self.shared.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
            DropPolicy::DropOldest => {
                queue.pop_front();
                queue.push_back(item);
                self.shared.dropped.fetch_add(1, Ordering::Relaxed);
                true
            }
        }
    }

    /// 当前丢弃计数。必须上报，见本模块文档。
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// 关闭通道，消费者 `drain` 完剩余事件后结束。
    pub fn close(&self) {
        self.shared.closed.store(true, Ordering::Release);
    }

    /// 是否已关闭。
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }

    /// 通道容量。
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.shared.capacity
    }
}

impl<T> BoundedReceiver<T> {
    /// 取出一条。
    pub fn recv(&self) -> Option<T> {
        let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.pop_front()
    }

    /// 批量取出（最多 `max` 条），减少锁竞争。
    pub fn recv_batch(&self, max: usize) -> Vec<T> {
        let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        let take = max.min(queue.len());
        queue.drain(..take).collect()
    }

    /// 当前积压条数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.shared
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 消费剩余事件（不阻塞）。用于会话收尾时把尾巴写进 .pbt。
    pub fn drain(&self) -> impl Iterator<Item = T> + '_ {
        std::iter::from_fn(|| self.recv())
    }

    /// 丢掉的条数。
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// 生产者是否已关闭。
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_oldest_keeps_tail() {
        let (tx, rx) = bounded::<u32>(3, DropPolicy::DropOldest);
        for i in 0..5 {
            assert!(tx.send(i));
        }
        assert_eq!(tx.dropped(), 2);
        assert_eq!(rx.drain().collect::<Vec<_>>(), vec![2, 3, 4]);
    }

    #[test]
    fn reject_policy_never_loses_silently() {
        let (tx, rx) = bounded::<u32>(1, DropPolicy::Reject);
        assert!(tx.send(1));
        assert!(!tx.send(2), "Reject 策略下写入应被显式拒绝");
        assert_eq!(tx.dropped(), 1);
        assert_eq!(rx.recv(), Some(1));
    }

    #[test]
    fn zero_capacity_is_promoted_to_one() {
        let (tx, rx) = bounded::<u32>(0, DropPolicy::DropNewest);
        assert_eq!(tx.capacity(), 1);
        assert!(tx.send(7));
        assert_eq!(rx.recv(), Some(7));
    }

    #[test]
    fn closed_channel_rejects_writes() {
        let (tx, rx) = bounded::<u32>(4, DropPolicy::DropNewest);
        tx.close();
        assert!(tx.is_closed() && rx.is_closed());
        assert!(!tx.send(1));
        assert!(rx.is_empty());
    }

    #[test]
    fn batch_never_exceeds_request() {
        let (tx, rx) = bounded::<u32>(64, DropPolicy::DropNewest);
        for i in 0..10 {
            tx.send(i);
        }
        assert_eq!(rx.recv_batch(4).len(), 4);
        assert_eq!(rx.len(), 6);
    }
}

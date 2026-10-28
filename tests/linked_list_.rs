//! 集成测试：用 `Retain<T>` 连接的双向循环链表验证确定性析构与 Scope 延迟清盘。
//!
//! 链表节点存放在 `Scope` 的 arena 中，节点之间用 `Retain<Node>` 指向前趋 / 后继。
//! 删除节点时只改邻居链接并释放外部句柄，不调用任何 `clear`；循环引用由
//! `Scope::close` 的统一清盘兜底。

use std::sync::Mutex;

use sw_scope::{Retain, Scope};

/// 同域用例的节点析构记录。
static SAME_SCOPE_DROPS: Mutex<std::vec::Vec<usize>> = Mutex::new(std::vec::Vec::new());

/// 同域链表节点。
struct Node {
    value: usize,
    prev: Option<Retain<Node>>,
    next: Option<Retain<Node>>,
}

impl Node {
    fn new(value: usize) -> Self {
        Node {
            value,
            prev: None,
            next: None,
        }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        SAME_SCOPE_DROPS
            .lock()
            .expect("同域析构记录锁不应中毒")
            .push(self.value);
    }
}

/// 跨域用例的节点析构记录。
static CROSS_SCOPE_DROPS: Mutex<std::vec::Vec<usize>> = Mutex::new(std::vec::Vec::new());

/// 跨域链表节点；与同域节点结构相同，但使用独立的计数器和 Drop 实现。
struct CrossNode {
    value: usize,
    prev: Option<Retain<CrossNode>>,
    next: Option<Retain<CrossNode>>,
}

impl CrossNode {
    fn new(value: usize) -> Self {
        CrossNode {
            value,
            prev: None,
            next: None,
        }
    }
}

impl Drop for CrossNode {
    fn drop(&mut self) {
        CROSS_SCOPE_DROPS
            .lock()
            .expect("跨域析构记录锁不应中毒")
            .push(self.value);
    }
}

/// 直接视图用例的节点析构记录。
static VIEW_DROPS: Mutex<std::vec::Vec<usize>> = Mutex::new(std::vec::Vec::new());

/// 带有 `Owning` / `Sharing` 视图的循环链表节点。
struct ViewNode {
    value: usize,
    prev: Option<Retain<ViewNode>>,
    next: Option<Retain<ViewNode>>,
}

impl ViewNode {
    fn new(value: usize) -> Self {
        ViewNode {
            value,
            prev: None,
            next: None,
        }
    }
}

impl Drop for ViewNode {
    fn drop(&mut self) {
        VIEW_DROPS
            .lock()
            .expect("直接视图析构记录锁不应中毒")
            .push(self.value);
    }
}

/// 把 `value` 追加为 `tail` 的后继，返回新节点的外部句柄。
///
/// - 手段：新节点先带 `prev = tail.clone()` 放入 arena，再把旧尾的 `next` 指向新节点。
/// - 判断：返回的句柄可继续作为下一轮的尾。
async fn append_node(scope: &Scope, tail: &Retain<Node>, value: usize) -> Retain<Node> {
    let node = scope
        .put_async(Node {
            value,
            prev: Some(tail.clone()),
            next: None,
        })
        .await
        .expect("节点放入应当成功");
    {
        let mut tail_owning = tail.try_owning().expect("尾节点应当可独占");
        tail_owning.next = Some(node.clone());
    }
    node
}

/// 从 `head` 沿 `next` 走 `steps` 步，返回目标节点的外部句柄。
fn find_node(head: &Retain<Node>, steps: usize) -> Retain<Node> {
    let mut current = head.clone();
    for _ in 0..steps {
        let next = {
            let owning = current.try_owning().expect("遍历中的节点应当可独占");
            owning.next.clone().expect("循环链表应当有后继")
        };
        current = next;
    }
    current
}

/// 把目标节点从双向链表中摘出，但不修改目标节点自己的 `prev` / `next` 字段。
fn unlink_node(node: &Retain<Node>) {
    let (prev, next) = {
        let owning = node.try_owning().expect("待删除节点应当可独占");
        (
            owning.prev.clone().expect("待删除节点应当有前趋"),
            owning.next.clone().expect("待删除节点应当有后继"),
        )
    };
    {
        let mut prev_owning = prev.try_owning().expect("前趋应当可独占");
        prev_owning.next = Some(next.clone());
    }
    {
        let mut next_owning = next.try_owning().expect("后继应当可独占");
        next_owning.prev = Some(prev.clone());
    }
}

/// 从 `head` 出发按 `next` 遍历 `count` 个节点，返回它们的值。
fn collect_values(head: &Retain<Node>, count: usize) -> std::vec::Vec<usize> {
    let mut values = std::vec::Vec::with_capacity(count);
    let mut current = head.clone();
    for _ in 0..count {
        let value = {
            let owning = current.try_owning().expect("遍历中的节点应当可独占");
            owning.value
        };
        values.push(value);
        let next = {
            let owning = current.try_owning().expect("遍历中的节点应当可独占");
            owning.next.clone().expect("循环链表应当有后继")
        };
        current = next;
    }
    values
}

/// 测试同一个 Scope 内由 `Retain` 组成的循环链表。
/// Test a circular linked list made of `Retain` handles inside one Scope.
///
/// - 手段：放入 6 个节点，连成双向环；删除一个中间节点并释放其外部句柄，再更新一个
///   节点值；最后释放 head 句柄，只留下节点之间的循环引用，调用 `Scope::close`。
///   / How: put six nodes and wire them into a circular doubly linked list; delete a middle
///   node and drop its external handle, update one value; finally drop the head handle so only
///   node-to-node cycles remain, then call `Scope::close`.
/// - 判断：删除节点后其 `Drop` 立即恰好执行一次；仅剩循环引用时析构计数不增加；
///   `Scope::close` 后其余节点各析构一次。
///   / What: the deleted node's `Drop` runs immediately exactly once; with only cycles left
///   the count does not grow; after `Scope::close` every remaining node is dropped once.
#[compio::test]
async fn same_scope_cycle_deleted_node_drops_now_and_scope_close_reclaims_rest() {
    SAME_SCOPE_DROPS
        .lock()
        .expect("同域析构记录锁不应中毒")
        .clear();
    let scope = Scope::new();

    let head = scope.put_async(Node::new(0)).await.expect("头节点放入");
    let mut tail = head.clone();
    for value in 1..6usize {
        tail = append_node(&scope, &tail, value).await;
    }
    // 首尾相接，形成真正的循环引用。
    {
        let mut head_owning = head.try_owning().expect("头节点应当可独占");
        head_owning.prev = Some(tail.clone());
    }
    {
        let mut tail_owning = tail.try_owning().expect("尾节点应当可独占");
        tail_owning.next = Some(head.clone());
    }
    drop(tail);

    // 删掉 value=2 的中间节点：先改邻居链接，再释放它唯一的外部句柄。
    let removed = find_node(&head, 2);
    unlink_node(&removed);
    drop(removed);
    assert_eq!(
        SAME_SCOPE_DROPS
            .lock()
            .expect("同域析构记录锁不应中毒")
            .as_slice(),
        &[2],
        "被删除节点必须在最后一个外部句柄消失时立即析构"
    );

    // 查改：更新一个存活节点，并验证剩余链表顺序。
    {
        let node_one_handle = find_node(&head, 1);
        let mut node_one = node_one_handle.try_owning().expect("节点 1 可独占");
        node_one.value = 101;
    }
    let mut expected = std::vec![0usize, 101, 3, 4, 5];
    expected.sort_unstable();
    let mut actual = collect_values(&head, 5);
    actual.sort_unstable();
    assert_eq!(actual, expected, "删除后的链表顺序应当正确");

    // 释放最后一个外部句柄：只剩节点之间的循环引用。
    drop(head);
    assert_eq!(
        SAME_SCOPE_DROPS
            .lock()
            .expect("同域析构记录锁不应中毒")
            .as_slice(),
        &[2],
        "仅剩循环引用时不应有节点被误析构"
    );

    // 不调用任何 clear，直接关闭 Scope。
    scope.close();
    let mut dropped = SAME_SCOPE_DROPS
        .lock()
        .expect("同域析构记录锁不应中毒")
        .clone();
    dropped.sort_unstable();
    let mut expected = std::vec![0usize, 2, 101, 3, 4, 5];
    expected.sort_unstable();
    assert_eq!(
        dropped, expected,
        "Scope::close 必须释放循环中的全部节点（含此前删除的节点）"
    );
}

/// 测试跨多个 Scope 的循环链表。
/// Test a circular linked list whose nodes are allocated by different Scopes.
///
/// - 手段：创建 3 个 Scope，把 6 个节点轮流放入不同 Scope，并连成跨域双向环；删除一个
///   中间节点，更新一个节点，释放所有外部句柄后依次 `Scope::close`。
///   / How: create three Scopes and put six nodes round-robin, wire them into a cross-scope
///   circular list; delete a middle node, update one node, drop all external handles, then
///   close every Scope.
/// - 判断：删除节点立即析构；只剩跨域循环时计数不变；所有 Scope 关闭后每个剩余节点各
///   析构一次。
///   / What: the deleted node drops immediately; the count stays flat with only cross-scope
///   cycles; after closing all Scopes every remaining node is dropped once.
#[compio::test]
async fn cross_scope_cycle_deleted_node_drops_now_and_all_scopes_close_reclaim_rest() {
    CROSS_SCOPE_DROPS
        .lock()
        .expect("跨域析构记录锁不应中毒")
        .clear();
    let scopes: std::vec::Vec<Scope> = (0..3).map(|_| Scope::new()).collect();
    let mut nodes: std::vec::Vec<Retain<CrossNode>> = std::vec::Vec::new();
    for value in 0..6usize {
        let node = scopes[value % scopes.len()]
            .put_async(CrossNode::new(value))
            .await
            .expect("跨域节点放入");
        nodes.push(node);
    }
    // 跨域双向环：每个节点都指向不同 Scope 上的邻居。
    for (index, node) in nodes.iter().enumerate() {
        let mut owning = node.try_owning().expect("跨域节点应当可独占");
        owning.prev = Some(nodes[(index + nodes.len() - 1) % nodes.len()].clone());
        owning.next = Some(nodes[(index + 1) % nodes.len()].clone());
    }

    // 删除 value=2 的节点，它位于 scopes[2]。
    let removed = nodes.remove(2);
    let (prev, next) = {
        let owning = removed.try_owning().expect("待删除跨域节点应当可独占");
        (
            owning.prev.clone().expect("应当有前趋"),
            owning.next.clone().expect("应当有后继"),
        )
    };
    {
        let mut prev_owning = prev.try_owning().expect("前趋应当可独占");
        prev_owning.next = Some(next.clone());
    }
    {
        let mut next_owning = next.try_owning().expect("后继应当可独占");
        next_owning.prev = Some(prev.clone());
    }
    drop(removed);
    assert_eq!(
        CROSS_SCOPE_DROPS
            .lock()
            .expect("跨域析构记录锁不应中毒")
            .as_slice(),
        &[2],
        "跨域被删除节点必须在最后一个外部句柄消失时立即析构"
    );

    // 查改一个存活节点。
    {
        let mut first = nodes[0].try_owning().expect("节点 0 应当可独占");
        first.value = 100;
    }

    // 释放所有外部句柄；跨域循环仍然存在。
    drop(nodes);
    assert_eq!(
        CROSS_SCOPE_DROPS
            .lock()
            .expect("跨域析构记录锁不应中毒")
            .as_slice(),
        &[2],
        "仅剩跨域循环引用时不应有节点被误析构"
    );

    // 依次关闭所有 Scope；节点数据由各自 Scope 负责析构。
    for scope in scopes {
        scope.close();
    }
    let mut dropped = CROSS_SCOPE_DROPS
        .lock()
        .expect("跨域析构记录锁不应中毒")
        .clone();
    dropped.sort_unstable();
    let mut expected = std::vec![100usize, 1, 2, 3, 4, 5];
    expected.sort_unstable();
    assert_eq!(
        dropped, expected,
        "所有 Scope::close 之后跨域循环中的节点必须全部析构（含此前删除的节点）"
    );
}

/// 测试 `close` 清除仅由 `Retain` 持有的节点，同时保留 `Owning` / `Sharing` 目标。
/// Test that `close` clears Retain-only nodes while keeping `Owning` / `Sharing` targets.
///
/// - 手段：建立 5 节点循环链表，对节点 1 取 `Owning`、节点 3 取 `Sharing` 并克隆一次；
///   释放其余外部 `Retain` 后调用 `Scope::close`。在视图存活期间读取它们、通过它们持有的
///   死 `Retain` 尝试升级，并依次释放 Sharing 克隆、Owning、最后一个 Sharing。
///   / How: build a five-node circular list, take `Owning` on node 1 and `Sharing` on node 3
///   (plus one clone), drop every other external `Retain`, then call `Scope::close`. While the
///   views live, read them, try to upgrade their now-dead `Retain` fields, and release the
///   Sharing clone, the Owning, and the last Sharing in order.
/// - 判断：`close` 立即析构节点 0/2/4（仅 Retain 持有），但保留节点 1/3；视图仍可读取，
///   它们持有的 `Retain` 升级失败（目标已清除），且不会重复析构。Owning 释放后节点 1
///   被清除，最后一个 Sharing 释放后节点 3 被清除，最终 5 个节点各析构一次。
///   / What: `close` immediately drops nodes 0/2/4 (Retain-only) but keeps 1/3; the views stay
///   readable, upgrades through their Retain fields fail because the targets are gone, and no
///   drop is duplicated. Releasing the Owning clears node 1; releasing the last Sharing clears
///   node 3, so all five nodes are dropped exactly once.
#[compio::test]
async fn close_clears_retain_only_nodes_but_keeps_owning_and_sharing() {
    VIEW_DROPS
        .lock()
        .expect("直接视图析构记录锁不应中毒")
        .clear();
    let scope = Scope::new();
    let mut nodes: std::vec::Vec<Retain<ViewNode>> = std::vec::Vec::new();
    for value in 0..5usize {
        nodes.push(
            scope
                .put_async(ViewNode::new(value))
                .await
                .expect("视图用例节点放入"),
        );
    }
    // 连成 5 节点双向环：每个节点都持有前后两个 Retain。
    for (index, node) in nodes.iter().enumerate() {
        let mut owning = node.try_owning().expect("建链节点应当可独占");
        owning.prev = Some(nodes[(index + nodes.len() - 1) % nodes.len()].clone());
        owning.next = Some(nodes[(index + 1) % nodes.len()].clone());
    }

    // 保留这两个外部 Retain，供 Owning / Sharing 借用。
    let retain_owning = nodes[1].clone();
    let retain_sharing = nodes[3].clone();
    let owning = retain_owning.try_owning().expect("节点 1 应当可独占");
    let sharing = retain_sharing.try_sharing().expect("节点 3 应当可共享");
    let sharing_clone = sharing.clone();

    // 其余外部句柄全部释放；只剩节点之间的循环引用与上面两个外部 Retain。
    drop(nodes);

    // 直接视图存活，但只有 Retain 持有的 0/2/4 仍然必须被清除。
    scope.close();
    let mut dropped = VIEW_DROPS
        .lock()
        .expect("直接视图析构记录锁不应中毒")
        .clone();
    dropped.sort_unstable();
    assert_eq!(
        dropped,
        std::vec![0usize, 2, 4],
        "close 必须清除仅由 Retain 持有的节点"
    );

    assert_eq!(owning.value, 1, "Owning 指向的数据必须仍然有效");
    assert_eq!(sharing.value, 3, "Sharing 指向的数据必须仍然有效");
    assert_eq!(sharing_clone.value, 3, "Sharing 克隆必须仍然有效");

    // 视图持有的 Retain 目标已经被 close 清除：升级必须失败，而不是 UB。
    let dead_next_of_owning = owning.next.as_ref().expect("节点 1 必须持有后继 Retain");
    assert!(
        dead_next_of_owning.try_owning().is_none(),
        "指向已清除节点的 Retain 不能升级为 Owning"
    );
    let dead_prev_of_owning = owning.prev.as_ref().expect("节点 1 必须持有前趋 Retain");
    assert!(
        dead_prev_of_owning.try_sharing().is_none(),
        "指向已清除节点的 Retain 不能升级为 Sharing"
    );
    let dead_prev_of_sharing = sharing.prev.as_ref().expect("节点 3 必须持有前趋 Retain");
    assert!(
        dead_prev_of_sharing.try_owning().is_none(),
        "Sharing 持有的死 Retain 同样不能升级"
    );
    // 死 Retain 仍可安全 clone / drop，引用计数不应被破坏。
    let dead_clone = dead_next_of_owning.clone();
    drop(dead_clone);
    assert_eq!(
        VIEW_DROPS
            .lock()
            .expect("直接视图析构记录锁不应中毒")
            .as_slice()
            .len(),
        3,
        "读取死 Retain 不得触发新的析构"
    );

    // 释放一个 Sharing 克隆：最后一个 Sharing 仍在，节点 3 继续保留。
    drop(sharing_clone);
    assert_eq!(
        VIEW_DROPS
            .lock()
            .expect("直接视图析构记录锁不应中毒")
            .as_slice()
            .len(),
        3,
        "还有 Sharing 时不得析构节点 3"
    );

    // 释放 Owning：节点 1 不再有直接视图，close 的重试立即清除它。
    drop(owning);
    let mut dropped = VIEW_DROPS
        .lock()
        .expect("直接视图析构记录锁不应中毒")
        .clone();
    dropped.sort_unstable();
    assert_eq!(
        dropped,
        std::vec![0usize, 1, 2, 4],
        "Owning 释放后节点 1 应被清除，节点 3 仍由 Sharing 保留"
    );

    // 最后一个 Sharing 释放：节点 3 被清除，5 个节点全部完成析构。
    drop(sharing);
    let mut dropped = VIEW_DROPS
        .lock()
        .expect("直接视图析构记录锁不应中毒")
        .clone();
    dropped.sort_unstable();
    assert_eq!(
        dropped,
        std::vec![0usize, 1, 2, 3, 4],
        "最后一个直接视图释放后所有节点必须各析构一次"
    );

    // 外部 Retain 仍在，但它们只是失效句柄；释放它们不得重复析构。
    drop(retain_owning);
    drop(retain_sharing);
    let mut dropped = VIEW_DROPS
        .lock()
        .expect("直接视图析构记录锁不应中毒")
        .clone();
    dropped.sort_unstable();
    assert_eq!(
        dropped,
        std::vec![0usize, 1, 2, 3, 4],
        "释放失效 Retain 不得重复析构"
    );
}

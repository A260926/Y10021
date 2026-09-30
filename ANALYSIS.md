# similar：五类 diff 算法到 `DiffOp`、compact 与 merge 的归一链路

本文档跟踪 `Algorithm::Myers`、`Patience`、`Histogram`、`Lcs`、`Hunt`
五种算法（`RawMyers` 是 Myers 的无启发式变体，走同一归一链路，在相关处一并说明）
从 hook 事件到公开 iterator 的每一个正规化步骤。所有结论都有
`tests/pipeline_tracker.rs` 中的定向测试逐条兜底；第 6 节给出
“结论 -> 用例 -> 对应代码分支”的核对表，审查者可以从单个用例回到对应分支与不变量。

本文不新造实现：追踪程序只调用仓库现有的公开入口
（`algorithms::diff_slices_deadline`、`algorithms::{Capture, Replace, Compact}`、
`capture_diff[_slices][_deadline]`、`DiffOp::iter_changes/iter_slices`、
`TextDiff`、`TextMerge`、`unified_diff`），不复制算法。

## 1. 归一链路总览

五种算法模块（`src/algorithms/{myers,patience,histogram,lcs,hunt}.rs`）
只依赖一个 trait 对象 `D: DiffHook`（`src/algorithms/hook.rs`），
在搜索过程中产出四种原始事件：`equal / delete / insert / replace`。
归一全部发生在 hook 装饰器与 `DiffOp` 转换层：

```text
算法内部（myers/patience/histogram/lcs/hunt）
  │  产出原始事件：equal / delete / insert（长度任意、可能逐 cell、可能 cursor 不对齐）
  ▼
S1 Capture                       src/algorithms/capture.rs
  │  事件 1:1 转成 DiffOp，不做任何合并
  ▼
S2 Replace                       src/algorithms/replace.rs
  │  相邻 delete+insert 簇 -> DiffOp::Replace；合并相邻同类事件
  ▼
S3 Compact::finish               src/algorithms/compact.rs
  │  cleanup_diff_ops：跨相等元素移动编辑边界、合并同 tag、规范化 cursor
  ▼
S4 公开 DiffOp 切片               src/common.rs::capture_diff[_slices][_deadline]
  │  Compact::new(Replace::new(Capture::new()))，into_inner 链取出 Vec<DiffOp>
  ▼
S5 公开消费端
     DiffOp::iter_changes/iter_slices   src/types.rs, src/iter.rs
     TextDiff::ops/grouped_ops/iter_all_changes  src/text/mod.rs
     UnifiedDiff hunk 范围               src/udiff.rs
     TextMerge regions（三方 merge）      src/merge.rs
```

`capture_diff` 的装配方式是唯一的（`src/common.rs:54`）：

```rust
let mut d = Compact::new(Replace::new(Capture::new()), old, new);
diff_deadline(alg, &mut d, ...).unwrap();
d.into_inner().into_inner().into_ops()
```

即 **S3+S2+S1 一次 finish 完成**。追踪程序把三层拆开分别捕获，
是为了让“哪一步改变了结构”可观察；生产代码没有第二条并行链路。

## 2. 各算法在 S1 产出的原始匹配块长什么样

原始流在结构上已经是 *end-covering*（所有 old/new 元素都被某个 op 覆盖，
长度总和正确），但**不保证 cursor 连续、不保证同 tag 已合并**。
这正是需要后续正规化的原因。以下是追踪程序在固定语料上观察到的真实形态
（语料定义见 `corpus()`，对应测试 `test_all_algorithms_survive_every_normalization_stage`）。

### 2.1 LCS：逐 table-cell 事件，相邻 Equal 不合并

经典 LCS 回溯（`src/algorithms/lcs.rs:159` 起）每走一格发一个事件。
`[1,2,1] -> [2,1,2]` 的 S1 是 4 个 op，其中两个 `Equal(len=1)` 相邻：

```text
Delete old=0..1 new=0..0
Equal  old=1..2 new=0..1
Equal  old=2..3 new=1..2     <- 与上一个同 tag，S1 允许
Insert old=3..3 new=2..3
```

`[1,2,3,4,5,6] -> [4,5,6,1,2,3]`（整块移动）的 S1 更碎：
三个逐格 `Delete`、三个逐格 `Equal`、一个 `Insert(len=3)`。
这些碎片在 S2 被 Replace 的 `equal` 累加逻辑
（`src/algorithms/replace.rs` 中 `self.eq = ... (eq_len + len)`）合并成整块。
定向反证：`test_replace_fuses_per_cell_lcs_events`、
`test_interleaved_block_move_normalizes_everywhere`。

### 2.2 Myers / Patience / Histogram / Hunt：锚点 + 递归区间

- Myers 的 middle-snake 递归在每个叶子发 equal/delete/insert；
  prefix/suffix trim（`src/algorithms/myers.rs:300` 起）直接发整块 equal。
- Patience 先找双方都唯一的锚点（`unique(...)`），锚点之间用 Myers 递归
  （`src/algorithms/patience.rs:110` 起）；外层包了一个 `Replace::new(Patience{..})`，
  所以锚点间 delete+insert 在算法内部就已部分熔合。
- Histogram 选频次最低的 anchor（`find_anchor`，
  `src/algorithms/histogram.rs:469`），找不到 anchor 时发
  `delete` 整段 + `insert` 整段（`SearchResult::None` 分支），
  deadline 超时则退化到 Myers（`SearchResult::Fallback`）。
- Hunt 用 match list + 最长递增锚链（`build_match_list`/`hunt_anchors`，
  `src/algorithms/hunt.rs:341/385`），锚点之间同样委托 Myers；
  match list 建不出来（deadline）时整体委托 Myers。

重复行语料 `[1,1,2,1,2] -> [2,1,2,1,1]` 上五种算法选出的匹配块位置不同，
但归一后都满足同一组不变量（见第 3 节）。这就是“同成本候选”的直接体现：
存在多个等长公共子序列时，算法的 tie-break 策略决定锚点位置，
归一层不评判优劣，只保证结构合法、可重建。

### 2.3 同成本候选的两种方向都合法

整块移动 `[1,2,3,4,5,6] -> [4,5,6,1,2,3]` 有两个等价最短脚本：

```text
insert-first: Insert 0..3 / Equal old 0..3,new 3..6 / Delete old 3..6
delete-first: Delete 0..3 / Equal old 3..6,new 0..3 / Insert 3..6
```

实测 Myers/Patience/Histogram 选 insert-first，LCS 的表格回溯
（`table.get(new, old+1) >= table.get(new+1, old)`，相等时走 delete）
和 Hunt 的锚链发射选 delete-first。两种方向都通过全部结构与重建不变量。
定向用例：`test_interleaved_block_move_normalizes_everywhere`
（显式接受两种形态并固定每个算法的选择，防止无意改动）。

## 3. DiffOp 的 old/new range 如何连续覆盖输入

这是所有消费端（text/udiff/merge）成立的前提，也是追踪程序在每一阶段都检查的核心。

### 3.1 不变量（I1–I4）

对 diff 区间 `old_range = os..oe`、`new_range = ns..ne` 上的公开 op 序列：

- **I1 cursor 连续**：第 0 个 op 从 `(os, ns)` 开始；对相邻 op，
  `op[i].old_range().start == op[i-1].old_range().end`，
  new 侧同理；range 不倒退。
- **I2 tag/shape 一致**：`Equal` 双侧等长且非空；`Delete` 只消耗 old
  （new range 为空）；`Insert` 只消耗 new；`Replace` 双侧都非空。
  代码见 `DiffOp::as_tag_tuple`（`src/types.rs`）。
- **I3 同 tag 不相邻**（S2 及以后）：相邻 equal run / delete / insert 必须已熔合。
  S1 原始流豁免（LCS 逐格事件）。
- **I4 覆盖到尾**：最后一个 op 的 old/new end 恰为 `oe`/`ne`，不重不漏。
- **I5 可重建**：按 `iter_slices` 顺序，Equal 块同时追加进 old/new，
  Delete 只进 old、Insert 只进 new，拼出的序列与原输入逐元素相等。

### 3.2 空端点

- `old`、`new` 都为空：每个算法直接返回，op 列表为空（I1–I4 对空列表平凡成立）。
- 仅一侧为空：算法在入口的 `is_empty_range` 分支发单个
  `Insert`/`Delete`（如 `src/algorithms/lcs.rs:112`、
  `src/algorithms/histogram.rs:366`、`src/algorithms/hunt.rs:212`、
  `src/algorithms/myers.rs:325`、patience `:139`）。
- 子区间 offset：range 是相对**整个输入切片的绝对索引**，不是相对 diff 子区间。
  非零起点（如 `1..5`）上第一个 op 必须从 1 开始、最后一个到 5 结束。

定向用例：`corpus()` 中的 `both_empty / old_empty / new_empty / edge_only`
（`test_all_algorithms_survive_every_normalization_stage`），
以及 `test_subrange_ops_keep_absolute_offsets`。

### 3.3 S1 原始流的一个已知“非连续”例外：deadline 兜底的 delete+insert

deadline 已过期时，LCS 建表返回 `None`（`src/algorithms/lcs.rs:278/327`），
尾部兜底发

```text
Delete old=0..N new=0..0
Insert old=N..N new=0..M     <- old 侧 cursor 直接落在 N（尾部），不是 0
```

Myers/Patience/Histogram/Hunt 的兜底 delete+insert 则把 Insert 的 old cursor 放在 0。
因此 S1 流在单侧 cursor 上可能“跳跃”，但它仍然 *end-covering*
（两侧元素总数正确、range 不越界）。**修复发生在 S3**：
`normalize_diff_op_cursors`（`src/algorithms/compact.rs` 中）
维护 old/new 双游标，重写 Insert 的 `old_index` 与 Delete 的 `new_index`，
随后 Replace 把这一对熔成一个 `Replace 0..N / 0..M`。
定向用例：`test_expired_deadline_degrades_to_replace_without_breaking_coverage`
同时断言 LCS 的 raw insert old_index 等于 `old.len()`，并断言公开结果是
从 0 开始的单个 Replace——这条用例直接反证“deadline 退化会破坏结构”的猜测。

## 4. Compact 何时会移动边界

`cleanup_diff_ops`（`src/algorithms/compact.rs`）做两件事：
`shift_all_diff_ops` 两遍扫描（先 Delete 后 Insert），
然后 `normalize_diff_op_cursors` 统一游标。

### 4.1 移动规则（只跨“相等”的元素）

对 `Insert` 紧邻 `Equal` 的情形（`shift_diff_ops_up`）：

1. 算 `common_suffix_len(old[prev_equal.old_range], new[this_insert.new_range])`；
2. 后缀相等长度 `k>0` 时，把 Insert 向左移 `k`、把左侧 Equal 左端缩 `k`，
   并在右侧（或并入下一个 Equal）补一个长 `k` 的 Equal；
3. 被缩空的 Equal 删除。下移（`shift_diff_ops_down`）对称，用 common prefix。

因此 compact **移动的是“编辑与相等段之间的分界线”**：
当边界两侧元素相等时，把相等元素划给 Equal，编辑挪到真正不等的位置。
Delete 的上下移还额外要求能与另一侧的 Delete 合并
（`can_merge_with_previous_delete` / `can_merge_with_next_delete`），
避免无意义地挪动纯删除。Insert/Delete 相邻时通过
`swap_adjacent_insert_delete` 交换并修补两侧 cursor。

### 4.2 最小反例（最容易猜错的结论）

`[1,1,0,2] -> [2,1,0,0,0,0]`，Histogram 的 S2（未 compact）是：

```text
Replace old=0..1 new=0..1     （old[0]=1 对 new[0]=2）
Equal   old=1..3 new=1..3     （[1,0] 相等）
Replace old=3..4 new=3..6
```

注意第一个 Replace 覆盖的 old[0]=1 与 new[0]=2 不等，但
**跨过边界**的 old[1]=1 与 new[1]=1 相等。compact 之后边界移动为：

```text
Insert  old=0..0 new=0..1     （只剩 new[0]=2）
Equal   old=0..1 new=1..2     （相等的 1 被划进 Equal）
Delete  old=1..2 new=2..2     （old 侧多出的 1）
Equal   old=2..3 new=2..3
Replace old=3..4 new=3..6
```

操作数没变少，但边界从“0”挪到了真正失配的位置——这正是语义清理。
如果没有相等元素可跨（`[1,2,3] -> [4,5,6]`），compact 是恒等映射，
所有算法都只剩单个 Replace。

定向用例：`test_compact_moves_boundary_across_repeated_items`
（逐 op 固定 S1/S2/S3 三阶段结构）、`test_compact_is_identity_without_common_items`。

### 4.3 大 op 列表走 gap buffer，但结果必须与向量路径一致

op 数 `<= 64`（`IN_PLACE_COMPACT_MAX_OPS`）时原地移位，
超过则用 `OpsBuffer`（双栈 gap buffer，编辑在 cursor 旁 O(1)），
避免大量 op 时的二次方移位。两条路径产出必须相同，
仓库自带 `test_gap_buffer_matches_vector_compaction` 做随机等价性。
追踪程序另外用 200 项细粒度交替输入（LCS 的 S1 流 >64 op）断言
gap-buffer 路径产出同样连续、可重建：
`test_compact_large_op_list_uses_gap_buffer_path`。

## 5. Deadline 如何退化，又为什么不破坏结构

### 5.1 各算法的退化策略

- **Myers**：递归 conquer 中周期性检查 deadline（`src/algorithms/myers.rs:518` 起）；
  超时接受当前最佳（可能非最短）split；preflight 在 deadline 已过期时
  直接跳过 hash 约简（`preflight::should_scan` 第一行返回 false）。
- **Patience**：建 unique 索引前先跑 shared preflight；deadline 过期时
  跳过“相同输入快速判断”（patience `:136`），锚点间递归把 deadline 透传给 Myers。
- **Histogram**：anchor 搜索（含 snake 扩展，每 1023 项检查一次）
  超时返回 `SearchResult::Fallback`，该区间整体委托 Myers。
- **Hunt**：match list 或锚链搜索超时（`build_match_list:355`、
  `hunt_anchors:407/431`）返回 `None`，中段整体委托 Myers。
- **LCS**：增量建表每 1024 个 cell 检查一次，超时返回 `None`；
  回溯循环整段跳过，由尾部 delete+insert 兜底——这是退化最“狠”的算法
  （可能给出零匹配脚本），文档 `diff_deadline` 也明确如此。

### 5.2 为什么结构不被破坏

关键在于**前缀/后缀 trim 在 deadline 检查之前完成**：
`common_prefix_len/common_suffix_len` 是线性且无 deadline 的。
以 `[1,2,3,4,5,6,7,8] -> [1,2,9,9,5,6,7,8]` + 已过期 deadline 为例，
五种算法都产出

```text
Equal   0..2 / 0..2
Replace 2..4 / 2..4
Equal   4..8 / 4..8
```

退化只发生在“中段”，中段无论多粗糙，入口/出口都发的是覆盖完整中段的
delete+insert（或 Myers 的近似脚本），其 range 端点来自 trim 算出的确定 cursor，
因此 I1–I5 仍然成立。

定向用例（全部用**已过期的 `Instant`**，无 sleep、无外网、无随机等待）：

- `test_expired_deadline_degrades_to_replace_without_breaking_coverage`：
  小输入过期 -> 单个全幅 Replace；顺带钉住 LCS raw insert cursor 错位由 S3 修复。
- `test_expired_deadline_keeps_trimmed_anchors`：锚点在退化中保留。
- `test_expired_deadline_large_disjoint_inputs`：3000 项完全不相交输入 ->
  全幅单个 Replace（shared near-disjoint fast path 与 deadline 退化的共同区域）。
- `test_deterministic_property_sweep_with_expired_deadline`：
  200 组确定性小输入，全部算法过期产出仍满足 I1–I5。

### 5.3 错误不因 deadline/归一被吞掉

`capture_diff*` 对算法返回值 `.unwrap()` 是因为 `Capture` 的关联错误类型是
`Infallible`；换成自定义 hook 时错误必须原样穿透 Compact/Replace。
`test_hook_error_keeps_cause_offset_and_context` 用一个在第 2 个事件爆炸的
hook（错误体携带事件名与精确的 old/new offset），断言经过
`Compact<Replace<ExplodingHook>>` 后收到的仍是同一个错误、同一组 offset，
证明归一层不重写、不掩盖 hook 错误（符合“保留原 cause、精确 path/offset、
必要上下文”的要求）。

## 6. 消费端如何使用这些 range（S5）

- **`DiffOp::iter_changes` / `iter_slices`**（`src/types.rs`、`src/iter.rs`）：
  `Replace` 先吐 Delete 项再吐 Insert 项；Equal 的每项都同时带 old/new 索引。
  `test_public_iterator_changes_reconstruct_inputs` 在全部五算法上检查
  单侧索引只出现在对应 tag 上、两侧索引各自严格递增、拼回原输入。
- **`TextDiff::grouped_ops(n)` / udiff**（`src/common.rs::group_diff_ops`、
  `src/udiff.rs`）：按连续覆盖的 op 序列裁出 hunk，长 Equal 只保留首尾各 n 行；
  hunk 内部 op 仍相邻连续，hunk 之间允许“跳上下文”。
  `test_text_and_udiff_consume_normalized_ranges` 校验每组内部 cursor 连续，
  并断言两行独立改动产生两个 hunk（hunk 头行号与 `-b/+X`、`-f/+Y` 内容对应）。
- **三方 merge**（`src/merge.rs::capture_edits/build_regions`）：
  先对 base->ours、base->theirs 各跑一次 `capture_diff`，非 Equal 的 op
  用 `old_range/new_range` 转成 `SideEdit` 并把相邻 edit 合并
  （依赖 `previous.base.end == edit.base.start` 这种连续性）。
  `test_three_way_merge_consumes_disjoint_ranges` 让两边改不同行，
  五种算法都必须无冲突合并，并校验 regions 的 base range 无缝拼满整个 base。

## 7. 核对表：结论 -> 用例 -> 代码落点

| 文档结论 | 定向用例（tests/pipeline_tracker.rs） | 代码落点 |
| --- | --- | --- |
| 五算法 S1->S4 全阶段结构合法、可重建（含重复行/空端点） | `test_all_algorithms_survive_every_normalization_stage` | `corpus/run_stages/Stages::check`；各算法 `diff_deadline` |
| LCS 逐 cell 事件由 Replace 熔合（I3 的 S1 豁免） | `test_replace_fuses_per_cell_lcs_events` | `lcs.rs:159-204`；`replace.rs` equal/del/ins 累加 |
| 同成本候选两种方向都合法且被固定 | `test_interleaved_block_move_normalizes_everywhere` | LCS 回溯 tie-break `lcs.rs:164`；Hunt `emit_anchored_script` |
| compact 跨相等元素移动边界 | `test_compact_moves_boundary_across_repeated_items` | `compact.rs shift_diff_ops_up/down` |
| 无相等元素时 compact 恒等 | `test_compact_is_identity_without_common_items` | `common_suffix_len==0` 的 break 分支 |
| gap-buffer 大路径与向量路径一致、仍连续 | `test_compact_large_op_list_uses_gap_buffer_path` | `compact.rs:315` 阈值 64 与 `OpsBuffer` |
| range 是绝对索引，子区间 offset 保持 | `test_subrange_ops_keep_absolute_offsets` | `common.rs::capture_diff`；各算法入口 range 透传 |
| 过期 deadline 退化为全幅 Replace，LCS cursor 错位被 S3 修复 | `test_expired_deadline_degrades_to_replace_without_breaking_coverage` | `lcs.rs:278/327`；`compact.rs normalize_diff_op_cursors` |
| trim 锚点在退化中保留 | `test_expired_deadline_keeps_trimmed_anchors` | 各算法 `common_prefix/suffix_len` 先行 |
| 大规模不相交输入退化不产生残缺脚本 | `test_expired_deadline_large_disjoint_inputs` | `preflight.rs:11` 起；Myers/anchor 兜底层 |
| hook 错误保留 cause/offset/上下文 | `test_hook_error_keeps_cause_offset_and_context` | `Replace/Compact` 的 `?` 透传 |
| 公开 iterator 重建输入、索引严格递增 | `test_public_iterator_changes_reconstruct_inputs` | `types.rs iter_slices`；`iter.rs ChangesIter` |
| grouped ops/udiff hunk 消费连续 range | `test_text_and_udiff_consume_normalized_ranges` | `common.rs group_diff_ops`；`udiff.rs` |
| 三方 merge 依赖 range 连续性拼满 base | `test_three_way_merge_consumes_disjoint_ranges` | `merge.rs capture_edits/build_regions` |
| 重复小字母表 400 组输入五算法均合法 | `test_deterministic_property_sweep_repetitive_inputs` | 全链路 |
| 过期 deadline 下 200 组输入仍合法 | `test_deterministic_property_sweep_with_expired_deadline` | 各算法 deadline 分支 |

## 8. 追踪程序如何运行

```bash
cargo test --all-features --test pipeline_tracker          # 只跑本文档的定向用例
cargo test --all-features                                  # 全套（单测 + 定向 + doctest）
```

追踪程序把每一阶段物化成 `Vec<DiffOp>`（`run_stages`）：
S1 直接 `Capture`；S2 用 `Replace<Capture>`；S3 用 `Compact<Replace<Capture>>`
并触发 `finish()`；S4 调用公开 `capture_diff_slices_deadline`。
`Stages::check` 对四阶段分别调用 `assert_op_invariants`
（S1 传 `fused=false`，其余 `true`）与 `assert_rebuild`，并断言
S3 与 S4 逐 op 相等（证明公开入口没有额外暗改）。

## 9. 刻意保持不变的公开行为

- 不改任何算法的搜索、锚点选择与 tie-break；同成本候选的具体取向
  （LCS/Hunt 的 delete-first，其余 insert-first）被测试固定而非“纠正”。
- 不改 `DiffOp` 的枚举形状、字段语义与 `as_tag_tuple` 的 range 约定。
- 不改变 deadline 语义：LCS 仍可能退化到零匹配；过期判断仍用 `Instant`，
  没有为可测试性引入注入时钟或新 API。
- 不改变 compact 的移位策略与 64 的 gap-buffer 阈值。
- 不把 `finish()` 多次调用之类的实现细节改成新约定；追踪通过
  分别装配独立 hook 链来观察阶段，而不是要求算法支持“半 finish”。

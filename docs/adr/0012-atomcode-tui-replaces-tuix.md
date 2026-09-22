# atomcode-tui 替换 tuix

状态: 已决定(2026-09-12)。取代 [`0011`](./0011-atomcode-tui-is-the-headless-front-end.md)。
替换路线的分层与缺口清单见 [`0013`](./0013-agent-product-host-ui.md)。

## 背景

0011 把 `atomcode-tui` 定成「harness 的无头前端,不取代 tuix」,理由是账单:
tui 12,472 行对 tuix 117,499 行,9.4 倍;tuix 的控制方向靠 `CodingRuntimeHandle`
20 个具体方法,和 `atomcode_coding::` 类型 440 处耦合,桥不过来。

六天后,plexus 线的目标变了:**这条线要替换现有栈**,不再是并存的第二套。
0011 自己写了失效条件——「若目标变成替换现有栈,这条决策重议,账单是那三个
数字」——现在触发了。

用户原话:「不看 tuix,这个要被 tui 替换掉。」

## 决策

**`atomcode-tui` 是产品 UI 的去处。tuix 是被替换的对象,不是标尺。**

三条推论:

1. **不读 tuix 定需求。** 要什么功能,按产品 UI 该有什么来定,不按 tuix 有什么
   来抄。tuix 的 16 个模态、60 个命令是它自己的历史,不是验收清单。
2. **补功能只走行。** 每个新面板、命令集、键位、流生产者都是配置树里的一个
   `Plugin` 行(见 0010)。Host 继续只持有 surface、事件循环、布局仲裁、焦点。
   往 Host 里塞功能是回到 tuix 的路。
3. **tuix 侧的 20 个方法是 harness 的欠账,不是 UI 的。** snapshot / restore /
   rewind_points / undo_to_prompt / mcp_tools / withdraw_mcp_tools /
   reassemble_provider / deactivate_provider / context_stats 这些要以 harness
   的行做出来,UI 再消费;不做 tuix→harness 的桥。

## 0011 里继续有效的部分

* 「直接用 tuix」不是省事的选项,那段分析仍然成立——它正是这次选择重写而
  不是桥接的理由。
* 三层结构、两条闸门、`--demo`、无头端到端:这些是替换路线能走的前提,
  每个新行落地当天就能在无 tty 下测。
* 9.4 倍那三个数字留着,用途从「反对理由」改成「工作量估算的起点」。

## 不再成立的部分

* 「不是产品 UI、不会变成产品 UI」——撤销。
* 「不再往 tuix 的外观上对齐」——改成「不以 tuix 为标尺」。外观按 tui 自己的
  设计走,和 tuix 像不像不是评价指标。
* 「不搬那 16 个模态、60 个命令」——改成「不搬,按需重做」。哪些要,由产品
  需求定,不由 tuix 的清单定。

## 失效条件

* 若替换路线中途叫停、plexus 线回到并存,退回 0011。

// 市场的「有没有新版本」判断，以及分组/筛选——从组件里抽出来单独测。
//
// 版本号在这套插件里长得很不统一：`3.12.10`、`2025-09-13`、`2.1.1`、`1.0.0`。所以比较
// 不能只按数字段做，也不能字符串比大小（`"0.9.10" < "0.9.9"` 就是错的）。规则：按 `.`
// 与 `-` 切段，两段都是数字就按数值比，否则按字典序；缺的段当 0。

export interface MarketVersioned {
  version: string;
}

/** `a` 与 `b` 的顺序：>0 表示 a 更新，0 相等，<0 更旧。 */
export function compareVersions(a: string, b: string): number {
  const left = a.split(/[.-]/);
  const right = b.split(/[.-]/);
  const length = Math.max(left.length, right.length);
  for (let index = 0; index < length; index++) {
    const l = left[index] ?? '0';
    const r = right[index] ?? '0';
    const ln = /^\d+$/.test(l) ? Number(l) : null;
    const rn = /^\d+$/.test(r) ? Number(r) : null;
    if (ln !== null && rn !== null) {
      // 只返回符号：差多少不是调用方要的信息，`3.12.10` vs `3.12.2` 差 8 会误导断言。
      if (ln !== rn) return ln > rn ? 1 : -1;
      continue;
    }
    if (l !== r) return l < r ? -1 : 1;
  }
  return 0;
}

/** 装了旧版时返回市场里的新版本号，否则 null（同版本、或装了更新的版本）。 */
export function upgradeFrom(installed: string | null | undefined, market: string): string | null {
  if (!installed) return null;
  if (!market) return null;
  return compareVersions(market, installed) > 0 ? market : null;
}

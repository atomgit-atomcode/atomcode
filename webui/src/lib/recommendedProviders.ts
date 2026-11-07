// 「装完这些才算能用」的那一组。
//
// APK 里不带任何运行时：bash、make、编译器、ninja 全都要从市场装。市场页是一张长
// 列表，新装的人没有任何线索知道哪几个是「不装就什么都干不了」——于是他们装了个
// python-data（几百 MB），然后发现 `pip install` 报 `gcc: not found`。
//
// 这份名单就是文档里已经论证过的 A+B 档（合计约 37 MB），顺序按「解开的洞最多」排：
// 先有 bash，再能解压，再能构建。

import type { MarketHelper } from '../api';

export interface RecommendedProvider {
  /** Market 里的名字，也就是 `MarketHelper.name`。 */
  name: string;
  /** 面板上显示的那句「为什么需要它」。 */
  reason: string;
}

/** A 档：不装的话连仓库里的脚本都跑不起来。 */
export const RECOMMENDED_PROVIDERS: RecommendedProvider[] = [
  { name: 'bash', reason: '仓库脚本写的是 #!/bin/bash，平台 shell 是 toybox' },
  { name: 'unzip', reason: '平台 unzip 读不了 Zip64 与加密条目' },
  { name: 'tar', reason: '平台 tar 没有 --transform / --exclude-from' },
  { name: 'pkgconf', reason: 'configure 与 CMake 的 find_package 按名字找它' },
  { name: 'tools', reason: 'make、C 编译器与随包 CLI' },
  { name: 'ninja', reason: 'CMake 默认要的构建工具' },
];

/**
 * 设备上还缺的推荐项，按名单顺序。
 *
 * `installedPackages` 为 `null` 表示这个页面**没法知道**（桌面浏览器没有宿主桥）。
 * 这种情况下什么都不报：说自己看不见的缺口，比不说话更糟——它会让人去装已经装了的。
 */
export function missingRecommended(
  providers: MarketHelper[],
  installedPackages: string[] | null,
): Array<RecommendedProvider & { provider: MarketHelper }> {
  if (!installedPackages) return [];
  const byName = new Map(providers.map((provider) => [provider.name, provider]));
  const missing: Array<RecommendedProvider & { provider: MarketHelper }> = [];
  for (const entry of RECOMMENDED_PROVIDERS) {
    const provider = byName.get(entry.name);
    // 市场没发布这个包：没有可点的东西，列出来只会让人找。
    if (!provider) continue;
    const id = provider.package ?? provider.name;
    if (installedPackages.includes(id)) continue;
    missing.push({ ...entry, provider });
  }
  return missing;
}

/** 提示行里用的名字串：`bash、unzip、tar`。 */
export function recommendedNames(
  missing: Array<RecommendedProvider & { provider: MarketHelper }>,
): string {
  return missing.map((entry) => entry.provider.title || entry.name).join('、');
}

/**
 * 落地区那条提示要不要出，以及出什么。
 *
 * 只有「确实缺」才返回内容：看不见（桌面浏览器没有宿主桥）或全都装了，都返回
 * `null`——在会话说第一句话的页面上报一个可能不存在的缺口，比不说更糟。
 */
export function landingRuntimesNotice(
  missing: Array<RecommendedProvider & { provider: MarketHelper }>,
): { count: number; names: string } | null {
  if (missing.length === 0) return null;
  return { count: missing.length, names: recommendedNames(missing) };
}

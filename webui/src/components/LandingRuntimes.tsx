import { useEffect, useState } from 'preact/hooks';
import { listMarket, listPackages, type MarketHelper } from '../api';
import { useT } from '../settings';
import {
  landingRuntimesNotice,
  missingRecommended,
  type RecommendedProvider,
} from '../lib/recommendedProviders';

type Missing = Array<RecommendedProvider & { provider: MarketHelper }>;

/**
 * Landing-page hint for the runtimes an APK cannot carry.
 *
 * The market is one long list and a fresh install has no way to know which few
 * packages make a repo script and a compiler work — people install a 300 MB data
 * bundle first and then meet `gcc: not found`. This says the same thing on the
 * page they actually land on, and leaves installing to the market, because every
 * provider needs one confirmation in Android's installer.
 *
 * Silent when it cannot tell: a desktop browser has no host bridge, so
 * `listPackages()` fails and the component claims nothing.
 */
export function LandingRuntimes({ onOpen }: { onOpen?: () => void }) {
  const t = useT();
  const [missing, setMissing] = useState<Missing | null>(null);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const [packages, market] = await Promise.all([listPackages(), listMarket()]);
        if (cancelled) return;
        const helpers = market.helpers ?? (market.helper ? [market.helper] : []);
        setMissing(missingRecommended(helpers, packages.map((entry) => entry.name)));
      } catch {
        if (!cancelled) setMissing([]);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const notice = missing ? landingRuntimesNotice(missing) : null;
  if (!notice) return null;
  return (
    <div class="landing-runtimes">
      <div class="landing-runtimes-text">
        {t('landing.runtimes', { count: notice.count, names: notice.names })}
      </div>
      {onOpen && (
        <button class="landing-runtimes-open" type="button" onClick={onOpen}>
          {t('landing.runtimesOpen')}
        </button>
      )}
    </div>
  );
}
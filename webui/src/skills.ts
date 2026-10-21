import { useEffect, useState } from 'preact/hooks';
import { getSkills } from './api';
import { createRefreshingList } from './refreshingList';

const skills = createRefreshingList(getSkills);
export const refreshSkills = () => skills.refresh();
export const invalidateSkills = () => skills.invalidate();

/** All skill browsers share updates; every opening requests a fresh list. */
export function useSkills(open: boolean) {
  const [snapshot, setSnapshot] = useState(skills.getSnapshot);
  useEffect(() => skills.subscribe(setSnapshot), []);
  useEffect(() => {
    if (open) void skills.refresh(true);
  }, [open]);
  return snapshot;
}

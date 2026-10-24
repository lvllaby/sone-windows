import { useCallback, useEffect, useRef, useState } from "react";

/** How long a slider keeps its own value after release, so a stale poll can't move it. */
const RELEASE_GRACE_MS = 1000;

/**
 * A slider value that follows `external` (e.g. polled Sonos state) except
 * while the user is dragging and for ~1 s after release.
 */
export function useHeldValue(external: number) {
  const [held, setHeld] = useState<number | null>(null);
  const pointerDown = useRef(false);
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);

  useEffect(() => () => clearTimeout(timer.current), []);

  const release = useCallback(() => {
    clearTimeout(timer.current);
    timer.current = setTimeout(() => setHeld(null), RELEASE_GRACE_MS);
  }, []);

  /** Call from the input's onChange with the new value. */
  const change = useCallback(
    (value: number) => {
      setHeld(value);
      // Keyboard changes have no pointer release to wait for
      if (!pointerDown.current) release();
    },
    [release],
  );

  /** Call from the input's onPointerDown. */
  const grab = useCallback(() => {
    pointerDown.current = true;
    clearTimeout(timer.current);
    const onUp = () => {
      pointerDown.current = false;
      window.removeEventListener("pointerup", onUp);
      window.removeEventListener("pointercancel", onUp);
      release();
    };
    window.addEventListener("pointerup", onUp);
    window.addEventListener("pointercancel", onUp);
  }, [release]);

  return { value: held ?? external, holding: held !== null, change, grab };
}

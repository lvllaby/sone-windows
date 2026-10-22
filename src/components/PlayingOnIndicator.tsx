import { memo } from "react";
import { useAtom, useAtomValue } from "jotai";
import { sonosPickerOpenAtom, sonosPlayingOnAtom } from "../atoms/sonos";

/** "Playing on <group>" while casting; opens the Sonos picker. */
const PlayingOnIndicator = memo(function PlayingOnIndicator() {
  const label = useAtomValue(sonosPlayingOnAtom);
  const [open, setOpen] = useAtom(sonosPickerOpenAtom);

  if (!label) return null;

  return (
    <button
      data-sonos-trigger
      onClick={() => setOpen(!open)}
      className="max-w-full truncate text-[10px] leading-none text-th-text-muted hover:text-th-accent transition-colors duration-150"
    >
      {label}
    </button>
  );
});

export default PlayingOnIndicator;

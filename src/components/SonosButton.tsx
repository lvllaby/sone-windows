import { memo, useCallback, useRef } from "react";
import { useAtom, useAtomValue } from "jotai";
import { Speaker } from "lucide-react";
import { playbackTargetAtom } from "../atoms/playback";
import { sonosGroupsAtom, sonosPickerOpenAtom } from "../atoms/sonos";
import SonosPicker from "./SonosPicker";

/** Opens the "Play on" picker; hidden while no Sonos group is known. */
const SonosButton = memo(function SonosButton() {
  const groups = useAtomValue(sonosGroupsAtom);
  const target = useAtomValue(playbackTargetAtom);
  const [open, setOpen] = useAtom(sonosPickerOpenAtom);
  const buttonRef = useRef<HTMLButtonElement>(null);

  const close = useCallback(
    (returnFocus: boolean) => {
      setOpen(false);
      if (returnFocus) buttonRef.current?.focus();
    },
    [setOpen],
  );

  if (groups.length === 0) return null;

  const casting = target.type === "sonos";

  return (
    <>
      <button
        ref={buttonRef}
        data-sonos-trigger
        onClick={() => setOpen(!open)}
        className={`${
          casting
            ? "text-th-accent"
            : "text-th-text-faint hover:text-th-text-primary"
        } transition-colors duration-150`}
        title="Sonos speakers"
        aria-label="Sonos speakers"
        aria-haspopup="dialog"
        aria-expanded={open}
      >
        <Speaker size={16} strokeWidth={2} />
      </button>
      {open && <SonosPicker anchorRef={buttonRef} onClose={close} />}
    </>
  );
});

export default SonosButton;

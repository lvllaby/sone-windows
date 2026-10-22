import { memo } from "react";
import { useAtom, useAtomValue } from "jotai";
import { Speaker } from "lucide-react";
import { playbackTargetAtom } from "../atoms/playback";
import { sonosGroupsAtom, sonosPickerAtom } from "../atoms/sonos";

/** Opens the "Play on" picker; hidden while no Sonos group is known. */
const SonosButton = memo(function SonosButton() {
  const groups = useAtomValue(sonosGroupsAtom);
  const target = useAtomValue(playbackTargetAtom);
  const [picker, setPicker] = useAtom(sonosPickerAtom);

  if (groups.length === 0) return null;

  const casting = target.type === "sonos";

  return (
    <>
      <button
        data-sonos-trigger
        onClick={(e) => setPicker(picker ? null : { opener: e.currentTarget })}
        className={`${
          casting
            ? "text-th-accent"
            : "text-th-text-faint hover:text-th-text-primary"
        } transition-colors duration-150`}
        title="Sonos speakers"
        aria-label="Sonos speakers"
        aria-haspopup="dialog"
        aria-expanded={!!picker}
      >
        <Speaker size={16} strokeWidth={2} />
      </button>
    </>
  );
});

export default SonosButton;

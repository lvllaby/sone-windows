import { memo } from "react";
import { useAtomValue } from "jotai";
import {
  currentTrackAtom,
  playbackTargetAtom,
  streamInfoAtom,
} from "../atoms/playback";

const QualityBadge = memo(function QualityBadge() {
  const currentTrack = useAtomValue(currentTrackAtom);
  const streamInfo = useAtomValue(streamInfoAtom);
  const casting = useAtomValue(playbackTargetAtom).type === "sonos";

  // The group decodes and plays the stream; SONE has no format details for it
  if (casting) {
    return (
      <span className="px-2 py-0.5 text-[9px] font-black rounded tracking-wider leading-none bg-th-button-hover text-th-text-primary">
        Sonos
      </span>
    );
  }

  const quality = streamInfo?.audioQuality || currentTrack?.audioQuality;
  if (!quality) return null;

  const isMax = quality === "HI_RES_LOSSLESS" || quality === "HI_RES";
  const isHiFi = quality === "LOSSLESS";

  const parts: string[] = [];
  if (streamInfo?.bitDepth) parts.push(`${streamInfo.bitDepth}-BIT`);
  if (streamInfo?.sampleRate) {
    const sr = streamInfo.sampleRate;
    parts.push(
      sr >= 1000
        ? `${(sr / 1000).toFixed(sr % 1000 === 0 ? 0 : 1)}kHz`
        : `${sr}Hz`,
    );
  }
  if (streamInfo?.codec) parts.push(streamInfo.codec);
  const detail = parts.join(" ");

  const label = isMax ? "HI-RES LOSSLESS" : isHiFi ? "LOSSLESS" : "HIGH";

  return (
    <div className="flex flex-col items-end gap-0.5">
      {detail && (
        <span className="text-[9px] text-th-text-faint font-medium tracking-wide inline">
          {detail}
        </span>
      )}
      <span
        className={`px-2 py-0.5 text-[9px] font-black rounded tracking-wider leading-none ${
          isMax
            ? "bg-th-accent text-black"
            : isHiFi
              ? "bg-th-accent/70 text-black"
              : "bg-th-button-hover text-th-text-primary"
        }`}
      >
        {label}
      </span>
    </div>
  );
});

export default QualityBadge;

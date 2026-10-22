import { atom } from "jotai";
import type { SonosGroup, SonosQueue, SonosState } from "../api/sonos";
import { playbackTargetAtom } from "./playback";

export const sonosGroupsAtom = atom<SonosGroup[]>([]);
export const sonosStateAtom = atom<SonosState | null>(null);
export const sonosQueueAtom = atom<SonosQueue | null>(null);

/** Sonos group volume, 0-1. */
export const sonosGroupVolumeAtom = atom(0);

/** Set while switching output; `to` is a coordinator id or "local". */
export const sonosHandoffAtom = atom<{ to: string } | null>(null);

/** Tracks inserted via "play next" since the current track started (Sonos has no manual queue). */
export const sonosPendingPlayNextAtom = atom(0);

/** The "Play on" picker: null when closed, else the element that opened it (the anchor). */
export const sonosPickerAtom = atom<{ opener: HTMLElement } | null>(null);

/** "Playing on <group>" while casting ("Reconnecting to <group>…" when the group stopped answering), else null. */
export const sonosPlayingOnAtom = atom((get) => {
  const target = get(playbackTargetAtom);
  if (target.type !== "sonos") return null;
  const state = get(sonosStateAtom);
  const reconnecting =
    state?.coordinatorId === target.coordinatorId &&
    state.reachability === "reconnecting";
  return reconnecting
    ? `Reconnecting to ${target.name}…`
    : `Playing on ${target.name}`;
});

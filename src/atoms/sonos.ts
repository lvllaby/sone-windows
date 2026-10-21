import { atom } from "jotai";
import type { SonosGroup, SonosQueue, SonosState } from "../api/sonos";

export const sonosGroupsAtom = atom<SonosGroup[]>([]);
export const sonosStateAtom = atom<SonosState | null>(null);
export const sonosQueueAtom = atom<SonosQueue | null>(null);

/** Sonos group volume, 0-1. */
export const sonosGroupVolumeAtom = atom(0);

/** Set while switching output; `to` is a coordinator id or "local". */
export const sonosHandoffAtom = atom<{ to: string } | null>(null);

/** Tracks inserted via "play next" since the current track started (Sonos has no manual queue). */
export const sonosPendingPlayNextAtom = atom(0);

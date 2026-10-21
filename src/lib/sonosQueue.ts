import type { QueueItem } from "../api/sonos";
import type { Track } from "../types";
import { isTrackUnavailable } from "./trackAvailability";

/** Sonos queue cap; the backend relies on start_index <= MAX_HISTORY. */
export const MAX_TOTAL = 1000;
const MAX_HISTORY = 50;

/**
 * Flattens SONE's queue model into the single list Sonos plays.
 *
 * Unavailable tracks are dropped (Sonos skips them anyway), except the
 * current track, which is always kept — the caller decides what to do with
 * it. History is trimmed to its last 50 entries and the total capped at
 * 1000. `startIndex` and every index into `trackIds` are what Sonos will
 * see; `overflow` is the upcoming tail that didn't fit.
 */
export function flattenForSonos(
  history: Track[],
  current: Track,
  manual: Track[],
  queue: Track[],
): { trackIds: number[]; startIndex: number; overflow: Track[] } {
  const hist = history
    .filter((t) => !isTrackUnavailable(t))
    .slice(-MAX_HISTORY);
  const upcoming = [...manual, ...queue].filter((t) => !isTrackUnavailable(t));
  const room = Math.max(0, MAX_TOTAL - hist.length - 1);
  return {
    trackIds: [...hist, current, ...upcoming.slice(0, room)].map((t) => t.id),
    startIndex: hist.length,
    overflow: upcoming.slice(room),
  };
}

/** Splits a mirrored Sonos track list around the current index. */
export function splitMirror(
  tracks: Track[],
  index: number,
): { history: Track[]; current: Track | null; queue: Track[] } {
  if (index < 0 || index >= tracks.length) {
    return { history: [], current: null, queue: [] };
  }
  return {
    history: tracks.slice(0, index),
    current: tracks[index],
    queue: tracks.slice(index + 1),
  };
}

/** Sonos queue index of the upcoming-queue entry `queueIndex`. */
export function queueIndexToSonos(
  currentIndex: number,
  queueIndex: number,
): number {
  return currentIndex + 1 + queueIndex;
}

/** Sonos index at which the next "play next" track goes. */
export function playNextInsertIndex(
  currentIndex: number,
  pending: number,
): number {
  return currentIndex + 1 + pending;
}

/**
 * Re-keys the pre-shuffle order onto the mirrored upcoming queue so that
 * turning shuffle off (which matches by `_qid`) works while casting: each
 * entry becomes the matching occurrence (by id) from `queue`, entries no
 * longer upcoming are dropped, and upcoming tracks it lacks go at the end.
 */
export function rekeyOriginalQueue(original: Track[], queue: Track[]): Track[] {
  const byId = new Map<number, Track[]>();
  for (const t of queue) {
    const list = byId.get(t.id);
    if (list) list.push(t);
    else byId.set(t.id, [t]);
  }
  const out: Track[] = [];
  for (const t of original) {
    const match = byId.get(t.id)?.shift();
    if (match) out.push(match);
  }
  const used = new Set(out);
  return [...out, ...queue.filter((t) => !used.has(t))];
}

/** Stand-in Track for a Sonos queue item that isn't (or can't be resolved as) a TIDAL track. */
export function placeholderTrack(item: QueueItem, index: number): Track {
  return {
    id: -(index + 1),
    title: item.kind === "tv" ? "TV" : item.title,
    duration: item.durationSecs ?? 0,
    artist: item.artist ? { id: 0, name: item.artist } : undefined,
    album: {
      id: 0,
      title: "",
      cover: item.albumArtUrl ?? undefined,
    },
  };
}

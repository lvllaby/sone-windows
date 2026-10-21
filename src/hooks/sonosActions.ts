/**
 * sonosActions — the Sonos side of usePlaybackActions. While the playback
 * target is a Sonos group, usePlaybackActions delegates here. The Sonos queue
 * is the source of truth and useSonosSync mirrors it back into the playback
 * atoms, so most methods just send the command.
 *
 * Indices: queueAtom index i is Sonos index `trackIndex + 1 + i`. Without a
 * trackIndex (TV input, radio) queue edits are no-ops; transport still works.
 * "Play next", "add to queue" and replacing the upcoming queue also need a
 * TIDAL current item. Every method is a no-op while a handoff is running.
 *
 * Queue state (pending "play next" block, overflow beyond the 1000-item cap)
 * is module-level, so every usePlaybackActions instance shares it.
 */

import { invoke } from "@tauri-apps/api/core";
import type { useStore } from "jotai";
import {
  SONOS_SN_UNKNOWN,
  sonosLoadQueue,
  sonosNext,
  sonosPause,
  sonosPlay,
  sonosPlayIndex,
  sonosPrevious,
  sonosQueueInsert,
  sonosQueueRemove,
  sonosQueueReplaceAfter,
  sonosSeek,
  sonosSetGroupVolume,
  sonosSetMute,
  sonosSetPlayMode,
  type QueueItem,
  type SonosQueue,
} from "../api/sonos";
import {
  sonosGroupVolumeAtom,
  sonosHandoffAtom,
  sonosPendingPlayNextAtom,
  sonosQueueAtom,
  sonosStateAtom,
} from "../atoms/sonos";
import {
  consecutiveFailCountAtom,
  currentTrackAtom,
  historyAtom,
  isPlayingAtom,
  manualQueueAtom,
  playbackTargetAtom,
  queueAtom,
} from "../atoms/playback";
import type { useToast } from "../contexts/ToastContext";
import { safeErrorMessage } from "../lib/errorUtils";
import { notifySeek } from "../lib/playbackPosition";
import {
  MAX_TOTAL,
  flattenForSonos,
  playNextInsertIndex,
  queueIndexToSonos,
} from "../lib/sonosQueue";
import { isTrackUnavailable } from "../lib/trackAvailability";
import type { Track } from "../types";
import { primeSonosTrackCache } from "./useSonosSync";

type Store = ReturnType<typeof useStore>;
type ShowToast = ReturnType<typeof useToast>["showToast"];

const VOLUME_THROTTLE_MS = 100;
const TOP_UP_CHUNK = 200;
/** A queue update this soon after our own edit may still predate it. */
const OWN_EDIT_GRACE_MS = 1500;
/** How long a load's start index overrides the (stale) polled index. */
const LOAD_INDEX_TTL_MS = 10_000;
const ERROR_TOAST_GAP_MS = 3000;

/** Upcoming tracks that didn't fit into the Sonos queue, appended near the end. */
let overflow: Track[] = [];
/** TIDAL id SONE last put at the end of the Sonos queue; overflow follows it. */
let overflowAnchor: number | null = null;
/** TIDAL ids of the "play next" block right after the current track, in order. */
let pendingIds: number[] = [];
/** Index the current track will have once a load's history prepend is done. */
let loadIndex: { index: number; at: number } | null = null;

let queueChain: Promise<unknown> = Promise.resolve();
let ownEditsInFlight = 0;
let lastOwnEditAt = 0;
let lastErrorToastAt = 0;

let volumeTimer: ReturnType<typeof setTimeout> | null = null;
let pendingVolume: number | null = null;

function setPending(store: Store, ids: number[]) {
  pendingIds = ids;
  store.set(sonosPendingPlayNextAtom, ids.length);
}

function loadWindowActive(): boolean {
  return loadIndex !== null && Date.now() - loadIndex.at < LOAD_INDEX_TTL_MS;
}

/** Runs queue edits one at a time, in call order, so their indices hold. */
function ownQueueEdit<T>(fn: () => Promise<T>): Promise<T> {
  const run = queueChain.then(async () => {
    ownEditsInFlight++;
    try {
      return await fn();
    } finally {
      ownEditsInFlight--;
      lastOwnEditAt = Date.now();
    }
  });
  queueChain = run.catch(() => {});
  return run;
}

/** Forgets the per-group queue state, e.g. when the target changes. */
export function resetSonosQueueState(store: Store) {
  overflow = [];
  overflowAnchor = null;
  loadIndex = null;
  setPending(store, []);
}

/**
 * Keeps the "play next" block in step when the Sonos index moves: advancing
 * consumes it, going back puts the skipped-back tracks in front of it.
 * `sameTrack` means the index moved under the same track (items inserted or
 * removed before it), which leaves the block alone. Call on every state.
 */
export function trackPendingIndexChange(
  store: Store,
  prev: number | null,
  next: number | null,
  sameTrack: boolean,
) {
  if (loadIndex) {
    // The load's history prepend moves the index; that's not playback.
    if (next === loadIndex.index || !loadWindowActive()) loadIndex = null;
    return;
  }
  if (prev === next || pendingIds.length === 0 || sameTrack) return;
  if (prev == null || next == null) {
    setPending(store, []);
    return;
  }
  if (next > prev) {
    setPending(store, pendingIds.slice(next - prev));
    return;
  }
  const back = (store.get(sonosQueueAtom)?.items ?? [])
    .slice(next + 1, prev + 1)
    .map((item) => item.tidalTrackId);
  if (back.length !== prev - next || back.some((id) => id == null)) {
    setPending(store, []);
  } else {
    setPending(store, [...(back as number[]), ...pendingIds]);
  }
}

/** Resets the "play next" block when a queue change SONE didn't make broke it. */
export function reconcilePendingWithQueue(store: Store, q: SonosQueue) {
  if (pendingIds.length === 0 || loadWindowActive()) return;
  if (ownEditsInFlight > 0 || Date.now() - lastOwnEditAt < OWN_EDIT_GRACE_MS) {
    return;
  }
  const index = store.get(sonosStateAtom)?.trackIndex;
  if (index == null) {
    setPending(store, []);
    return;
  }
  const block = q.items.slice(index + 1, index + 1 + pendingIds.length);
  if (
    block.length !== pendingIds.length ||
    block.some((item, k) => item.tidalTrackId !== pendingIds[k])
  ) {
    setPending(store, []);
  }
}

export function createSonosActions(store: Store, showToast: ShowToast) {
  const locked = () => store.get(sonosHandoffAtom) !== null;

  const fail = (error: unknown) => {
    console.error("[sonos] command failed:", error);
    // Volume drags and quick taps against a dead group would stack toasts
    const now = Date.now();
    if (now - lastErrorToastAt < ERROR_TOAST_GAP_MS) return;
    lastErrorToastAt = now;
    const target = store.get(playbackTargetAtom);
    const group = target.type === "sonos" ? target.name : "Sonos";
    showToast(`Couldn't reach ${group}`, "error");
  };

  /** Sends one command; false (after toasting) when it failed. */
  const send = async (fn: () => Promise<void>): Promise<boolean> => {
    try {
      await fn();
      return true;
    } catch (error) {
      fail(error);
      return false;
    }
  };

  /**
   * Sonos index of the current track, or null when not playing from the
   * queue (or, with `needTidal`, when the current item isn't TIDAL).
   */
  const currentIndex = (needTidal: boolean): number | null => {
    const target = store.get(playbackTargetAtom);
    const state = store.get(sonosStateAtom);
    if (target.type !== "sonos") return null;
    if (loadWindowActive() && loadIndex) {
      // Polled state lags a fresh load; the load's start index is what Sonos has.
      if (state?.trackIndex !== loadIndex.index) return loadIndex.index;
    }
    if (!state || state.coordinatorId !== target.coordinatorId) return null;
    if (state.trackIndex == null) return null;
    if (needTidal && state.currentItem?.kind !== "tidal") return null;
    return state.trackIndex;
  };

  /** Runs a queue edit with the Sonos index current at that moment. */
  const editQueue = async (
    needTidal: boolean,
    fn: (index: number) => Promise<void>,
  ): Promise<boolean> => {
    try {
      return await ownQueueEdit(async () => {
        const index = currentIndex(needTidal);
        if (index == null) return false;
        await fn(index);
        return true;
      });
    } catch (error) {
      fail(error);
      return false;
    }
  };

  /** Appends to the end of the Sonos queue; what doesn't fit goes to the overflow front. */
  const appendToSonos = (tracks: Track[]) =>
    editQueue(false, async (index) => {
      // The polled queue is exact once our own edits have shown up in it;
      // until then the (optimistically updated) mirror is the better guess.
      const q = store.get(sonosQueueAtom);
      const target = store.get(playbackTargetAtom);
      const settled =
        !loadWindowActive() && Date.now() - lastOwnEditAt >= OWN_EDIT_GRACE_MS;
      const length =
        settled &&
        q &&
        target.type === "sonos" &&
        q.coordinatorId === target.coordinatorId
          ? q.items.length
          : index + 1 + store.get(queueAtom).length;
      const room = Math.max(0, MAX_TOTAL - length);
      const fit = tracks.slice(0, room);
      if (tracks.length > room) overflow = [...tracks.slice(room), ...overflow];
      if (fit.length === 0) return;
      await sonosQueueInsert(
        fit.map((t) => t.id),
        length,
      );
      overflowAnchor = fit[fit.length - 1].id;
    });

  const playable = (tracks: Track[]) =>
    tracks.filter((t) => t.id > 0 && !isTrackUnavailable(t));

  const sendVolume = (level: number) => {
    void send(() => sonosSetGroupVolume(Math.round(level * 100)));
  };

  return {
    isCasting: () => store.get(playbackTargetAtom).type === "sonos",

    /** Replaces the Sonos queue with history/current/manual/queue from the atoms. */
    async loadFromAtoms(play: boolean, positionSecs = 0): Promise<void> {
      if (locked()) return;
      const current = store.get(currentTrackAtom);
      if (!current || current.id <= 0) return;
      // Mirrored placeholders (non-TIDAL items) have no id Sonos could load
      const history = store.get(historyAtom).filter((t) => t.id > 0);
      const manual = store.get(manualQueueAtom).filter((t) => t.id > 0);
      const queue = store.get(queueAtom).filter((t) => t.id > 0);
      const flat = flattenForSonos(history, current, manual, queue);
      primeSonosTrackCache([...history, current, ...manual, ...queue]);
      overflow = flat.overflow;
      overflowAnchor = flat.trackIds[flat.trackIds.length - 1];
      setPending(store, []);
      store.set(consecutiveFailCountAtom, 0);
      try {
        await ownQueueEdit(() =>
          sonosLoadQueue(flat.trackIds, flat.startIndex, positionSecs, play),
        );
        loadIndex = { index: flat.startIndex, at: Date.now() };
      } catch (error) {
        overflow = [];
        if (safeErrorMessage(error, "") === SONOS_SN_UNKNOWN) {
          console.error("[sonos] load failed:", error);
          showToast(
            "Play any TIDAL track once from the Sonos app, then retry",
            "error",
          );
        } else {
          fail(error);
        }
      }
    },

    async pause(): Promise<void> {
      if (locked()) return;
      if (await send(sonosPause)) store.set(isPlayingAtom, false);
    },

    async resume(): Promise<void> {
      if (locked()) return;
      if (await send(sonosPlay)) store.set(isPlayingAtom, true);
    },

    /** `afterError`: an automatic skip, which keeps the fail counter running. */
    async next(opts?: { afterError?: boolean }): Promise<void> {
      if (locked()) return;
      if (!opts?.afterError) store.set(consecutiveFailCountAtom, 0);
      loadIndex = null;
      await send(sonosNext);
    },

    async previous(): Promise<void> {
      if (locked()) return;
      store.set(consecutiveFailCountAtom, 0);
      loadIndex = null;
      await send(sonosPrevious);
    },

    async seek(positionSecs: number): Promise<void> {
      if (locked()) return;
      if (!(await send(() => sonosSeek(positionSecs)))) return;
      notifySeek(positionSecs);
      if (store.get(sonosStateAtom)?.currentItem?.kind === "tidal") {
        invoke("notify_track_seeked").catch(() => {});
      }
    },

    async playIndex(queueIndex: number): Promise<void> {
      if (locked()) return;
      const index = currentIndex(false);
      if (index == null) return;
      if (queueIndex < 0 || queueIndex >= store.get(queueAtom).length) return;
      store.set(consecutiveFailCountAtom, 0);
      loadIndex = null;
      await send(() => sonosPlayIndex(queueIndexToSonos(index, queueIndex)));
    },

    /** "Play next": right after the current track, ahead of earlier "play next"s. */
    async insertNext(tracks: Track[]): Promise<void> {
      if (locked() || currentIndex(true) == null) return;
      const add = playable(tracks);
      if (add.length === 0) return;
      const ids = add.map((t) => t.id);
      primeSonosTrackCache(add);
      const before = store.get(queueAtom);
      const optimistic = [...add, ...before];
      store.set(queueAtom, optimistic);
      setPending(store, [...ids, ...pendingIds]);
      const ok = await editQueue(true, (index) =>
        sonosQueueInsert(ids, index + 1),
      );
      if (!ok) {
        if (store.get(queueAtom) === optimistic) store.set(queueAtom, before);
        setPending(store, []);
      }
    },

    /** "Add to queue": after the "play next" block, ahead of the rest. */
    async insertAfterPending(tracks: Track[]): Promise<void> {
      if (locked() || currentIndex(true) == null) return;
      const add = playable(tracks);
      if (add.length === 0) return;
      const ids = add.map((t) => t.id);
      primeSonosTrackCache(add);
      const offset = pendingIds.length;
      const before = store.get(queueAtom);
      const at = Math.min(offset, before.length);
      const optimistic = [...before.slice(0, at), ...add, ...before.slice(at)];
      store.set(queueAtom, optimistic);
      setPending(store, [...pendingIds, ...ids]);
      const ok = await editQueue(true, (index) =>
        sonosQueueInsert(ids, playNextInsertIndex(index, offset)),
      );
      if (!ok) {
        if (store.get(queueAtom) === optimistic) store.set(queueAtom, before);
        setPending(store, []);
      }
    },

    /** Appends at the end (behind any overflow still waiting to go in). */
    async appendEnd(tracks: Track[]): Promise<void> {
      if (locked() || currentIndex(false) == null) return;
      const add = playable(tracks);
      if (add.length === 0) return;
      primeSonosTrackCache(add);
      if (overflow.length > 0) {
        overflow = [...overflow, ...add];
        return;
      }
      await appendToSonos(add);
    },

    /**
     * Top-up near the end of the queue: appends the next overflow chunk.
     * False when there's nothing to add (or the queue is no longer SONE's).
     */
    topUpFromOverflow(items: QueueItem[]): boolean {
      if (locked() || overflow.length === 0) return false;
      if (items[items.length - 1]?.tidalTrackId !== overflowAnchor) {
        // Another controller replaced the tail; our overflow doesn't follow it
        overflow = [];
        return false;
      }
      const chunk = overflow.slice(0, TOP_UP_CHUNK);
      overflow = overflow.slice(TOP_UP_CHUNK);
      void appendToSonos(chunk);
      return true;
    },

    async removeAt(queueIndex: number): Promise<void> {
      if (locked() || currentIndex(false) == null) return;
      const before = store.get(queueAtom);
      if (queueIndex < 0 || queueIndex >= before.length) return;
      const optimistic = before.filter((_, i) => i !== queueIndex);
      store.set(queueAtom, optimistic);
      if (queueIndex < pendingIds.length) {
        setPending(
          store,
          pendingIds.filter((_, i) => i !== queueIndex),
        );
      }
      const ok = await editQueue(false, (index) =>
        sonosQueueRemove(queueIndexToSonos(index, queueIndex)),
      );
      if (!ok && store.get(queueAtom) === optimistic) {
        store.set(queueAtom, before);
      }
    },

    /** Replaces everything after the current track (reorder, shuffle, clear). */
    async replaceUpcoming(tracks: Track[]): Promise<void> {
      if (locked() || currentIndex(true) == null) return;
      const next = playable(tracks);
      primeSonosTrackCache(next);
      setPending(store, []);
      await editQueue(true, async (index) => {
        const room = Math.max(0, MAX_TOTAL - index - 1);
        const fit = next.slice(0, room);
        overflow =
          tracks.length === 0 ? [] : [...next.slice(room), ...overflow];
        await sonosQueueReplaceAfter(
          index,
          fit.map((t) => t.id),
        );
        const state = store.get(sonosStateAtom);
        overflowAnchor =
          fit.length > 0
            ? fit[fit.length - 1].id
            : (state?.currentItem?.tidalTrackId ?? null);
      });
    },

    /** 0-1; one request per 100 ms, and the last value always goes out. */
    setVolume(level: number): void {
      if (locked()) return;
      const clamped = Math.max(0, Math.min(1, level));
      store.set(sonosGroupVolumeAtom, clamped);
      pendingVolume = clamped;
      if (volumeTimer !== null) return;
      const flush = () => {
        volumeTimer = null;
        if (pendingVolume === null) return;
        const value = pendingVolume;
        pendingVolume = null;
        sendVolume(value);
        volumeTimer = setTimeout(flush, VOLUME_THROTTLE_MS);
      };
      flush();
    },

    async setMuted(muted: boolean): Promise<void> {
      if (locked()) return;
      await send(() => sonosSetMute(muted));
    },

    /** `repeat`: 0 = off, 1 = repeat-all, 2 = repeat-one. */
    async setRepeat(repeat: number): Promise<void> {
      if (locked()) return;
      await send(() => sonosSetPlayMode(repeat));
    },
  };
}

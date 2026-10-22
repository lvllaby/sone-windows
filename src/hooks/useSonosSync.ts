/**
 * useSonosSync — keeps the Sonos atoms in sync with the backend events and,
 * while the playback target is a Sonos group, mirrors the Sonos queue and
 * state into the regular playback atoms (history / current / queue /
 * isPlaying / repeat). While the target is local, only the Sonos atoms
 * (groups, state, queue, group volume) are touched.
 *
 * While casting it also: sends repeat changes to Sonos, tops the queue up
 * (overflow, then autoplay) near its end, skips tracks Sonos can't play, and
 * drives scrobbling from the mirrored state.
 *
 * Mount once (AppInitializer). Exposes nothing.
 */

import { useEffect, useMemo } from "react";
import { useStore } from "jotai";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  sonosGetSnapshot,
  type QueueItem,
  type SonosGroup,
  type SonosQueue,
  type SonosState,
  type TransportState,
} from "../api/sonos";
import { getMixItems, getTrack } from "../api/tidal";
import {
  sonosGroupsAtom,
  sonosGroupVolumeAtom,
  sonosHandoffAtom,
  sonosQueueAtom,
  sonosStateAtom,
} from "../atoms/sonos";
import {
  allowExplicitAtom,
  autoplayAtom,
  consecutiveFailCountAtom,
  currentTrackAtom,
  historyAtom,
  isPlayingAtom,
  manualQueueAtom,
  originalQueueAtom,
  playbackTargetAtom,
  queueAtom,
  repeatAtom,
  shuffleAtom,
} from "../atoms/playback";
import { useToast } from "../contexts/ToastContext";
import {
  placeholderTrack,
  rekeyOriginalQueue,
  splitMirror,
} from "../lib/sonosQueue";
import { isTrackUnavailable } from "../lib/trackAvailability";
import type { Track } from "../types";
import {
  createSonosActions,
  reconcilePendingWithQueue,
  resetSonosQueueState,
  trackPendingIndexChange,
} from "./sonosActions";
import {
  MAX_CONSECUTIVE_PLAY_FAILS,
  notifyTrackStarted,
} from "./usePlaybackActions";

const MAX_CONCURRENT_RESOLVES = 6;
/** A track that has played this long on Sonos counts as a successful play. */
const PLAYED_OK_SECS = 3;

/** Resolved TIDAL tracks by id; survives queue updates so only new ids hit the API. */
const resolvedTracks = new Map<number, Track>();

/**
 * Seeds the resolved-track cache with tracks SONE is about to send, so the
 * mirror shows them without a getTrack round-trip. Queue-only fields
 * (`_qid`, `_source`, …) are dropped; the mirror stamps its own.
 */
export function primeSonosTrackCache(tracks: Track[]) {
  for (const t of tracks) {
    if (t.id <= 0 || resolvedTracks.has(t.id)) continue;
    const clean = Object.fromEntries(
      Object.entries(t).filter(([key]) => !key.startsWith("_")),
    ) as unknown as Track;
    if (!clean.artist && clean.artists?.[0]) clean.artist = clean.artists[0];
    resolvedTracks.set(t.id, clean);
  }
}

/**
 * One Track per queue item, in Sonos order and with the same length, so
 * indices line up with Sonos. Unresolvable items become placeholders.
 */
async function resolveItems(items: QueueItem[]): Promise<Track[]> {
  const out: Track[] = items.map((item, i) => {
    const cached =
      item.kind === "tidal" && item.tidalTrackId != null
        ? resolvedTracks.get(item.tidalTrackId)
        : undefined;
    return cached ?? placeholderTrack(item, i);
  });

  const misses: number[] = [];
  items.forEach((item, i) => {
    if (
      item.kind === "tidal" &&
      item.tidalTrackId != null &&
      !resolvedTracks.has(item.tidalTrackId)
    ) {
      misses.push(i);
    }
  });

  let next = 0;
  const worker = async () => {
    while (next < misses.length) {
      const i = misses[next++];
      const id = items[i].tidalTrackId as number;
      try {
        const track = resolvedTracks.get(id) ?? (await getTrack(id));
        resolvedTracks.set(id, track);
        out[i] = track;
      } catch {
        // keep the placeholder; not cached so a later update retries
      }
    }
  };
  await Promise.all(
    Array.from(
      { length: Math.min(MAX_CONCURRENT_RESOLVES, misses.length) },
      worker,
    ),
  );
  return out;
}

/**
 * Per-position copies with a `_qid` built from id + occurrence, so queue
 * entries keep their identity across mirror updates (shuffle off matches
 * by `_qid`).
 */
function withMirrorQids(tracks: Track[]): Track[] {
  const seen = new Map<number, number>();
  return tracks.map((t) => {
    const n = seen.get(t.id) ?? 0;
    seen.set(t.id, n + 1);
    return { ...t, _qid: `sonos-${t.id}-${n}` };
  });
}

declare global {
  interface Window {
    /** Dev-only: point the playback target at a Sonos group. */
    __sonosDebugSetTarget?: (coordinatorId: string, name: string) => void;
  }
}

export function useSonosSync() {
  const store = useStore();
  const { showToast } = useToast();
  const sonos = useMemo(
    () => createSonosActions(store, showToast),
    [store, showToast],
  );

  useEffect(() => {
    let disposed = false;
    let queueGen = 0;
    let mirrorTracks: Track[] = [];
    let lastRepeat: number | null = null;
    /** Last repeat mode exchanged with Sonos (reported or sent): never echoed back. */
    let knownRepeat: number | null = null;
    let lastSliceKey: string | null = null;
    let prevIndex: number | null = null;
    let prevTidalId: number | null = null;
    /** `trackIndex|tidalId` that notify_track_started was last sent (or skipped) for. */
    let startedPair: string | null = null;
    let lastTransport: TransportState | null = null;
    /** Queue updateId a top-up was last attempted for. */
    let topUpKey: number | null = null;
    let topUpBusy = false;

    const targetCoordinator = (): string | null => {
      const target = store.get(playbackTargetAtom);
      return target.type === "sonos" ? target.coordinatorId : null;
    };

    /** Writes history/current/queue from the resolved tracks and the current index. */
    const applyMirror = () => {
      const coordinatorId = targetCoordinator();
      const state = store.get(sonosStateAtom);
      if (!coordinatorId || !state || state.coordinatorId !== coordinatorId) {
        return;
      }
      if (state.trackIndex == null) {
        // TV input etc.: not in the queue, so leave history/queue alone
        if (state.currentItem) {
          store.set(currentTrackAtom, placeholderTrack(state.currentItem, 0));
        }
        return;
      }
      const { history, current, queue } = splitMirror(
        mirrorTracks,
        state.trackIndex,
      );
      if (!current) return; // queue not loaded yet
      store.set(historyAtom, history);
      store.set(currentTrackAtom, current);
      store.set(queueAtom, queue);
      store.set(manualQueueAtom, []);
      const original = store.get(originalQueueAtom);
      if (store.get(shuffleAtom) && original) {
        store.set(originalQueueAtom, rekeyOriginalQueue(original, queue));
      }
    };

    const resolveAndMirror = async (q: SonosQueue | null) => {
      const gen = ++queueGen;
      const coordinatorId = targetCoordinator();
      if (!q || !coordinatorId || q.coordinatorId !== coordinatorId) return;
      const tracks = await resolveItems(q.items);
      // A newer queue (or a target change) arrived while resolving
      if (disposed || gen !== queueGen) return;
      mirrorTracks = withMirrorQids(tracks);
      applyMirror();
    };

    /** notify_track_started for a new cast play, once the track is resolved. */
    const notifyCastStarted = async (id: number, pair: string) => {
      let track = resolvedTracks.get(id);
      if (!track) {
        try {
          track = await getTrack(id);
          resolvedTracks.set(id, track);
        } catch {
          return;
        }
      }
      // Moved on (or a handoff started) while resolving
      if (disposed || startedPair !== pair || store.get(sonosHandoffAtom)) {
        return;
      }
      notifyTrackStarted(track, false);
    };

    /**
     * Scrobbling: a new (index, TIDAL id) pair that is playing is a new play,
     * so a repeat of the same track at a new index counts again; playing ↔
     * paused on that play maps to paused/resumed. Placeholders never count.
     * `shiftedFrom` is the previous pair when the index only moved under the
     * same track (load prepend, edits before it): that play carries over.
     */
    const scrobble = (
      state: SonosState,
      tidalId: number | null,
      shiftedFrom: string | null,
    ) => {
      const pair = `${state.trackIndex}|${tidalId}`;
      const transport = state.transportState;
      if (shiftedFrom !== null && startedPair === shiftedFrom) {
        startedPair = pair;
      }
      if (store.get(sonosHandoffAtom)) {
        // The handoff itself isn't a play: note where we are, send nothing
        startedPair = pair;
        if (transport !== "transitioning") lastTransport = transport;
        return;
      }
      if (transport === "playing" && pair !== startedPair) {
        startedPair = pair;
        lastTransport = transport;
        if (tidalId != null) void notifyCastStarted(tidalId, pair);
        return;
      }
      if (pair === startedPair && tidalId != null) {
        if (lastTransport === "playing" && transport === "paused") {
          invoke("notify_track_paused").catch(() => {});
        } else if (lastTransport === "paused" && transport === "playing") {
          invoke("notify_track_resumed").catch(() => {});
        }
      }
      if (transport !== "transitioning") lastTransport = transport;
    };

    /** Autoplay top-up: recommendations from the current track's mix (as the local path). */
    const appendRecommendations = async (
      coordinatorId: string,
      items: QueueItem[],
      currentId: number,
    ) => {
      const current =
        resolvedTracks.get(currentId) ?? (await getTrack(currentId));
      const trackMixId = current.mixes?.TRACK_MIX;
      if (!trackMixId) return;
      const { tracks: radio } = await getMixItems(trackMixId);
      const queued = new Set(items.map((item) => item.tidalTrackId));
      const explicitOk = store.get(allowExplicitAtom);
      const fresh = radio.filter(
        (t) =>
          !queued.has(t.id) &&
          (explicitOk || !t.explicit) &&
          !isTrackUnavailable(t),
      );
      if (fresh.length === 0) return;
      if (disposed || targetCoordinator() !== coordinatorId) return;
      await sonos.appendEnd(fresh);
    };

    /** Near the end of the queue: append the stashed overflow, else autoplay. */
    const maybeTopUp = (state: SonosState) => {
      const q = store.get(sonosQueueAtom);
      if (!q || q.coordinatorId !== state.coordinatorId) return;
      if (state.trackIndex == null || state.trackIndex < q.items.length - 2) {
        return;
      }
      if (topUpBusy || topUpKey === q.updateId || store.get(sonosHandoffAtom)) {
        return;
      }
      topUpKey = q.updateId;
      if (sonos.topUpFromOverflow(q.items)) return;
      const item = state.currentItem;
      // Repeat-all loops the queue; non-TIDAL content has nothing to base it on
      if (!store.get(autoplayAtom) || state.repeat === 1) return;
      if (item?.kind !== "tidal" || item.tidalTrackId == null) return;
      topUpBusy = true;
      appendRecommendations(state.coordinatorId, q.items, item.tidalTrackId)
        .catch(() => {})
        .finally(() => {
          topUpBusy = false;
        });
    };

    const applyState = (state: SonosState | null) => {
      const coordinatorId = targetCoordinator();
      if (!state || state.coordinatorId !== coordinatorId) return;
      store.set(isPlayingAtom, state.transportState === "playing");
      if (state.repeat !== lastRepeat) {
        lastRepeat = state.repeat;
        knownRepeat = state.repeat;
        store.set(repeatAtom, state.repeat);
      }

      const tidalId =
        state.currentItem?.kind === "tidal"
          ? state.currentItem.tidalTrackId
          : null;
      // Same track at a new index: a shift (prepend, edits before it) unless
      // playback moved forward onto a repeat of it (the old slot still holds
      // the same id).
      const q = store.get(sonosQueueAtom);
      const items = q?.coordinatorId === state.coordinatorId ? q.items : null;
      const replayed =
        prevIndex != null &&
        state.trackIndex != null &&
        state.trackIndex > prevIndex &&
        items?.[prevIndex]?.tidalTrackId === tidalId;
      const shifted =
        tidalId != null &&
        tidalId === prevTidalId &&
        prevIndex !== state.trackIndex &&
        !replayed;
      const shiftedFrom = shifted ? `${prevIndex}|${prevTidalId}` : null;
      trackPendingIndexChange(store, prevIndex, state.trackIndex, shifted);
      prevIndex = state.trackIndex;
      prevTidalId = tidalId;

      if (
        state.transportState === "playing" &&
        state.positionSecs >= PLAYED_OK_SECS
      ) {
        store.set(consecutiveFailCountAtom, 0);
      }
      scrobble(state, tidalId, shiftedFrom);

      // Re-slice only when the current track changed, not on every poll
      const key = `${state.trackIndex}|${state.currentItem?.title ?? ""}`;
      if (key !== lastSliceKey) {
        lastSliceKey = key;
        applyMirror();
      }
      maybeTopUp(state);
    };

    /** Sonos stopped on a track it couldn't play: skip it, up to the usual cap. */
    const onPlaybackError = () => {
      if (!targetCoordinator() || store.get(sonosHandoffAtom)) return;
      const fails = store.get(consecutiveFailCountAtom) + 1;
      if (fails >= MAX_CONSECUTIVE_PLAY_FAILS) {
        showToast("Multiple tracks failed to play — stopped", "error");
        store.set(consecutiveFailCountAtom, 0);
        store.set(isPlayingAtom, false);
        return;
      }
      store.set(consecutiveFailCountAtom, fails);
      const state = store.get(sonosStateAtom);
      const q = store.get(sonosQueueAtom);
      const atEnd =
        state?.trackIndex != null &&
        q != null &&
        state.repeat !== 1 &&
        state.trackIndex >= q.items.length - 1;
      if (atEnd) {
        // Nothing to skip to
        showToast("Track unavailable", "info");
        return;
      }
      showToast("Track unavailable on Sonos — skipping", "info");
      void sonos.next({ afterError: true });
    };

    const setGroups = (groups: SonosGroup[]) => {
      if (!disposed) store.set(sonosGroupsAtom, groups);
    };

    // The backend's first discovery may have finished before we attached.
    sonosGetSnapshot()
      .then((snap) => {
        if (store.get(sonosGroupsAtom).length === 0) setGroups(snap.groups);
      })
      .catch(() => {});

    const unlistens = [
      listen<SonosGroup[]>("sonos-topology", (e) => setGroups(e.payload)),
      listen<SonosState>("sonos-state", (e) => {
        store.set(sonosStateAtom, e.payload);
        store.set(sonosGroupVolumeAtom, e.payload.groupVolume / 100);
        applyState(e.payload);
      }),
      listen<SonosQueue>("sonos-queue", (e) => {
        store.set(sonosQueueAtom, e.payload);
        if (e.payload.coordinatorId === targetCoordinator()) {
          reconcilePendingWithQueue(store, e.payload);
        }
        void resolveAndMirror(e.payload);
      }),
      listen("sonos-playback-error", onPlaybackError),
    ];

    // PlayerBar / MPRIS set repeatAtom directly; forward user changes to Sonos.
    const unsubRepeat = store.sub(repeatAtom, () => {
      const repeat = store.get(repeatAtom);
      if (!targetCoordinator() || store.get(sonosHandoffAtom)) return;
      if (repeat === knownRepeat) return;
      knownRepeat = repeat;
      void sonos.setRepeat(repeat);
    });

    // Switching to a Sonos group: mirror what we already have right away.
    let prevCoordinator = targetCoordinator();
    const unsubTarget = store.sub(playbackTargetAtom, () => {
      const coordinatorId = targetCoordinator();
      if (coordinatorId === prevCoordinator) return;
      prevCoordinator = coordinatorId;
      lastRepeat = null;
      knownRepeat = null;
      lastSliceKey = null;
      prevIndex = null;
      prevTidalId = null;
      startedPair = null;
      lastTransport = null;
      topUpKey = null;
      mirrorTracks = [];
      queueGen++;
      resetSonosQueueState(store);
      if (!coordinatorId) return;
      applyState(store.get(sonosStateAtom));
      void resolveAndMirror(store.get(sonosQueueAtom));
    });

    if (import.meta.env.DEV) {
      window.__sonosDebugSetTarget = (coordinatorId, name) =>
        store.set(playbackTargetAtom, { type: "sonos", coordinatorId, name });
    }

    return () => {
      disposed = true;
      unsubTarget();
      unsubRepeat();
      unlistens.forEach((p) => p.then((fn) => fn()).catch(() => {}));
      if (import.meta.env.DEV) delete window.__sonosDebugSetTarget;
    };
  }, [store, showToast, sonos]);
}

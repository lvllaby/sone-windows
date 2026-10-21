/**
 * useSonosSync — keeps the Sonos atoms in sync with the backend events and,
 * while the playback target is a Sonos group, mirrors the Sonos queue and
 * state into the regular playback atoms (history / current / queue /
 * isPlaying / repeat). While the target is local, only the Sonos atoms
 * (groups, state, queue, group volume) are touched.
 *
 * Mount once (AppInitializer). Exposes nothing.
 */

import { useEffect } from "react";
import { useStore } from "jotai";
import { listen } from "@tauri-apps/api/event";
import {
  sonosGetSnapshot,
  type QueueItem,
  type SonosGroup,
  type SonosQueue,
  type SonosState,
} from "../api/sonos";
import { getTrack } from "../api/tidal";
import {
  sonosGroupsAtom,
  sonosGroupVolumeAtom,
  sonosQueueAtom,
  sonosStateAtom,
} from "../atoms/sonos";
import {
  currentTrackAtom,
  historyAtom,
  isPlayingAtom,
  manualQueueAtom,
  playbackTargetAtom,
  queueAtom,
  repeatAtom,
} from "../atoms/playback";
import { placeholderTrack, splitMirror } from "../lib/sonosQueue";
import type { Track } from "../types";

const MAX_CONCURRENT_RESOLVES = 6;

/** Resolved TIDAL tracks by id; survives queue updates so only new ids hit the API. */
const resolvedTracks = new Map<number, Track>();

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

declare global {
  interface Window {
    /** Dev-only: point the playback target at a Sonos group. */
    __sonosDebugSetTarget?: (coordinatorId: string, name: string) => void;
  }
}

export function useSonosSync() {
  const store = useStore();

  useEffect(() => {
    let disposed = false;
    let queueGen = 0;
    let mirrorTracks: Track[] = [];
    let lastRepeat: number | null = null;
    let lastSliceKey: string | null = null;

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
    };

    const resolveAndMirror = async (q: SonosQueue | null) => {
      const gen = ++queueGen;
      const coordinatorId = targetCoordinator();
      if (!q || !coordinatorId || q.coordinatorId !== coordinatorId) return;
      const tracks = await resolveItems(q.items);
      // A newer queue (or a target change) arrived while resolving
      if (disposed || gen !== queueGen) return;
      mirrorTracks = tracks;
      applyMirror();
    };

    const applyState = (state: SonosState | null) => {
      const coordinatorId = targetCoordinator();
      if (!state || state.coordinatorId !== coordinatorId) return;
      store.set(isPlayingAtom, state.transportState === "playing");
      if (state.repeat !== lastRepeat) {
        lastRepeat = state.repeat;
        store.set(repeatAtom, state.repeat);
      }
      // Re-slice only when the current track changed, not on every poll
      const key = `${state.trackIndex}|${state.currentItem?.title ?? ""}`;
      if (key !== lastSliceKey) {
        lastSliceKey = key;
        applyMirror();
      }
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
        void resolveAndMirror(e.payload);
      }),
    ];

    // Switching to a Sonos group: mirror what we already have right away.
    let prevCoordinator = targetCoordinator();
    const unsubTarget = store.sub(playbackTargetAtom, () => {
      const coordinatorId = targetCoordinator();
      if (coordinatorId === prevCoordinator) return;
      prevCoordinator = coordinatorId;
      lastRepeat = null;
      lastSliceKey = null;
      mirrorTracks = [];
      queueGen++;
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
      unlistens.forEach((p) => p.then((fn) => fn()).catch(() => {}));
      if (import.meta.env.DEV) delete window.__sonosDebugSetTarget;
    };
  }, [store]);
}

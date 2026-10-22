/**
 * useSonosHandoff — switches the playback target between This computer and
 * Sonos groups, carrying the current track, position and queue across
 * (spec §6). sonosHandoffAtom is set for the whole switch: Sonos actions and
 * cast scrobbling stand still meanwhile, and a second switch is ignored.
 *
 * useSonosAutoAttach (mount once, AppInitializer) reattaches to the
 * remembered group on startup and falls back to This computer when the
 * selected group is lost.
 */

import { useCallback, useEffect, useMemo } from "react";
import { useAtomValue, useStore } from "jotai";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  SONOS_SN_UNKNOWN,
  sonosGetSnapshot,
  sonosPause,
  sonosSelectGroup,
  sonosSetPlayMode,
  type SonosState,
} from "../api/sonos";
import {
  sonosGroupsAtom,
  sonosHandoffAtom,
  sonosQueueAtom,
  sonosStateAtom,
} from "../atoms/sonos";
import {
  currentTrackAtom,
  historyAtom,
  isPlayingAtom,
  manualQueueAtom,
  originalQueueAtom,
  pendingResumePositionAtom,
  playbackTargetAtom,
  queueAtom,
  repeatAtom,
  shuffleAtom,
  streamInfoAtom,
} from "../atoms/playback";
import { useToast } from "../contexts/ToastContext";
import { safeErrorMessage } from "../lib/errorUtils";
import { getInterpolatedPosition } from "../lib/playbackPosition";
import { isTrackUnavailable } from "../lib/trackAvailability";
import type { Track } from "../types";
import { createSonosActions } from "./sonosActions";
import { seekAfterStart, usePlaybackActions } from "./usePlaybackActions";

type Store = ReturnType<typeof useStore>;
type ShowToast = ReturnType<typeof useToast>["showToast"];

/** A selected group that sends no state at all this long counts as lost. */
const NO_STATE_TIMEOUT_MS = 30_000;

/** Startup reattach happens at most once per app session (survives remounts). */
let autoAttachDone = false;
let autoAttachBusy = false;

/** Mirrored non-TIDAL items (radio, TV, unresolvable) have ids <= 0. */
const isPlaceholder = (t: Track) => t.id <= 0;

const withoutPlaceholders = (tracks: Track[]) =>
  tracks.some(isPlaceholder) ? tracks.filter((t) => !isPlaceholder(t)) : tracks;

/** What a handoff needs to send, or to put back when it fails. */
interface Snapshot {
  current: Track | null;
  history: Track[];
  manual: Track[];
  queue: Track[];
  original: Track[] | null;
  repeat: number;
  pending: number | null;
  position: number;
  wasPlaying: boolean;
}

function takeSnapshot(store: Store): Snapshot {
  const pending = store.get(pendingResumePositionAtom);
  return {
    current: store.get(currentTrackAtom),
    history: store.get(historyAtom),
    manual: store.get(manualQueueAtom),
    queue: store.get(queueAtom),
    original: store.get(originalQueueAtom),
    repeat: store.get(repeatAtom),
    pending,
    // While casting this is the polled Sonos position
    position: pending ?? getInterpolatedPosition(),
    wasPlaying: store.get(isPlayingAtom),
  };
}

function restoreQueue(store: Store, snap: Snapshot) {
  store.set(currentTrackAtom, snap.current);
  store.set(historyAtom, snap.history);
  store.set(manualQueueAtom, snap.manual);
  store.set(queueAtom, snap.queue);
  store.set(originalQueueAtom, snap.original);
}

/**
 * Leaving a group: keeps the mirrored queue for This computer, minus the
 * placeholders it can't play. Returns the current track, or null when that
 * was a placeholder (nothing is current then).
 */
function keepMirroredQueue(store: Store): Track | null {
  store.set(historyAtom, withoutPlaceholders(store.get(historyAtom)));
  store.set(queueAtom, withoutPlaceholders(store.get(queueAtom)));
  const original = store.get(originalQueueAtom);
  if (original) store.set(originalQueueAtom, withoutPlaceholders(original));
  const current = store.get(currentTrackAtom);
  if (current && isPlaceholder(current)) {
    store.set(currentTrackAtom, null);
    return null;
  }
  return current;
}

/** Drops a group's polled state and queue so they can never be mirrored later. */
function forgetGroupState(store: Store) {
  store.set(sonosStateAtom, null);
  store.set(sonosQueueAtom, null);
}

/** Follows a group's own queue: SONE's manual queue and shuffle order don't apply. */
function enterMirrorMode(store: Store, coordinatorId: string, name: string) {
  store.set(manualQueueAtom, []);
  store.set(originalQueueAtom, null);
  store.set(shuffleAtom, false);
  store.set(playbackTargetAtom, { type: "sonos", coordinatorId, name });
}

function groupName(store: Store, coordinatorId: string): string {
  return (
    store.get(sonosGroupsAtom).find((g) => g.coordinatorId === coordinatorId)
      ?.name ?? "Sonos"
  );
}

function toastHandoffError(showToast: ShowToast, error: unknown, name: string) {
  console.error("[sonos] handoff failed:", error);
  if (safeErrorMessage(error, "") === SONOS_SN_UNKNOWN) {
    showToast(
      "Play any TIDAL track once from the Sonos app, then retry",
      "error",
    );
  } else {
    showToast(`Couldn't reach ${name}`, "error");
  }
}

const warn = (what: string) => (error: unknown) =>
  console.warn(`[sonos] ${what} failed:`, error);

function useSwitchTarget() {
  const store = useStore();
  const { showToast } = useToast();
  const { playTrack, pauseTrack, resumeTrack, seekTo } = usePlaybackActions();
  const sonos = useMemo(
    () => createSonosActions(store, showToast),
    [store, showToast],
  );

  /** This computer → group, or group → group (`fromGroup`). */
  const toGroup = useCallback(
    async (coordinatorId: string, fromGroup: boolean) => {
      const name = groupName(store, coordinatorId);
      const snap = takeSnapshot(store);
      // Unavailable tracks and placeholders count as nothing loaded
      const loadable =
        !!snap.current &&
        !isPlaceholder(snap.current) &&
        !isTrackUnavailable(snap.current);
      try {
        if (fromGroup) {
          // Leaving the old group: it pauses, its play stops counting
          await sonosPause().catch(warn("pause before switching"));
          await invoke("notify_track_stopped").catch(() => {});
          forgetGroupState(store);
        } else if (loadable && snap.wasPlaying) {
          await pauseTrack();
        }
        await sonosSelectGroup(coordinatorId, false);
        if (!loadable) {
          // Nothing to send: just follow what the group plays
          enterMirrorMode(store, coordinatorId, name);
          return;
        }
        // The mirror stands still during the handoff, so the atoms stay ours
        store.set(playbackTargetAtom, { type: "sonos", coordinatorId, name });
        const loaded = await sonos.loadFromAtoms(
          snap.wasPlaying,
          snap.position,
          { forHandoff: true },
        );
        if (!loaded) throw new Error("nothing to load");
        // Whatever arrived before the load finished is the group's old queue;
        // the poller reports the new one (and state) next
        forgetGroupState(store);
        // The load keeps the group's play mode; bring ours along
        await sonosSetPlayMode(snap.repeat).catch(warn("set play mode"));
        store.set(pendingResumePositionAtom, null);
        if (!fromGroup) {
          await invoke("stop_track").catch(warn("stop_track"));
          // stop_track also cleared SMTC/Discord; dropping the local stream
          // info (meaningless while casting) makes AppInitializer re-push them
          store.set(streamInfoAtom, null);
        }
      } catch (error) {
        toastHandoffError(showToast, error, name);
        store.set(playbackTargetAtom, { type: "local" });
        // Forget the failed group too, or the next launch reattaches to it
        await sonosSelectGroup(null, true).catch(warn("deselect"));
        forgetGroupState(store);
        restoreQueue(store, snap);
        store.set(repeatAtom, snap.repeat);
        if (fromGroup) {
          // The old group is paused already: carry on here, paused
          const current = keepMirroredQueue(store);
          store.set(isPlayingAtom, false);
          store.set(pendingResumePositionAtom, current ? snap.position : null);
        } else if (snap.wasPlaying) {
          await seekTo(snap.position);
          await resumeTrack();
        } else {
          store.set(isPlayingAtom, false);
          store.set(pendingResumePositionAtom, snap.pending);
        }
      }
    },
    [store, showToast, sonos, pauseTrack, resumeTrack, seekTo],
  );

  /** Group → This computer: the group pauses, playback continues here. */
  const toLocal = useCallback(async () => {
    const position = getInterpolatedPosition();
    const wasPlaying = store.get(isPlayingAtom);
    await invoke("notify_track_stopped").catch(() => {});
    await sonosPause().catch(warn("pause before switching"));
    await sonosSelectGroup(null, true).catch(warn("deselect"));
    forgetGroupState(store);
    store.set(playbackTargetAtom, { type: "local" });
    const current = keepMirroredQueue(store);
    if (!current) {
      store.set(isPlayingAtom, false);
      return;
    }
    if (wasPlaying) {
      const result = await playTrack(current, { skipHistoryPush: true });
      if (result.ok) {
        if (position > 0) await seekAfterStart(store, position);
        return;
      }
    }
    // Loaded on resume (resumeTrack seeks there once it has started)
    store.set(isPlayingAtom, false);
    store.set(pendingResumePositionAtom, position);
  }, [store, playTrack]);

  /** `to` is "local" (This computer) or a group's coordinator id. */
  return useCallback(
    async (to: "local" | string): Promise<void> => {
      if (store.get(sonosHandoffAtom)) return;
      const target = store.get(playbackTargetAtom);
      const already =
        to === "local"
          ? target.type === "local"
          : target.type === "sonos" && target.coordinatorId === to;
      if (already) return;
      store.set(sonosHandoffAtom, { to });
      try {
        if (to === "local") await toLocal();
        else await toGroup(to, target.type === "sonos");
      } catch (error) {
        console.error("[sonos] switch failed:", error);
      } finally {
        store.set(sonosHandoffAtom, null);
      }
    },
    [store, toLocal, toGroup],
  );
}

export function useSonosHandoff(): {
  switchTarget: (to: "local" | string) => Promise<void>;
  handoffInProgress: boolean;
} {
  const switchTarget = useSwitchTarget();
  const handoffInProgress = useAtomValue(sonosHandoffAtom) !== null;
  return { switchTarget, handoffInProgress };
}

export function useSonosAutoAttach() {
  const store = useStore();
  const { showToast } = useToast();

  // Reattach to the remembered group in mirror mode: no transport command.
  // Retried on each topology update until attached, or until local
  // playback has started.
  useEffect(() => {
    const eligible = () =>
      !autoAttachDone &&
      store.get(playbackTargetAtom).type === "local" &&
      !store.get(isPlayingAtom) &&
      !store.get(sonosHandoffAtom);

    const tryAttach = async () => {
      if (autoAttachBusy || !eligible()) return;
      autoAttachBusy = true;
      try {
        const snap = await sonosGetSnapshot();
        const coordinatorId = snap.attachable;
        if (!coordinatorId || !eligible()) return;
        await sonosSelectGroup(coordinatorId, false);
        if (!eligible()) {
          // Local playback started meanwhile (a switch owns the selection)
          if (
            store.get(playbackTargetAtom).type === "local" &&
            !store.get(sonosHandoffAtom)
          ) {
            await sonosSelectGroup(null, false).catch(warn("deselect"));
          }
          return;
        }
        const name =
          snap.groups.find((g) => g.coordinatorId === coordinatorId)?.name ??
          "Sonos";
        enterMirrorMode(store, coordinatorId, name);
      } catch (error) {
        console.warn("[sonos] auto-attach failed:", error);
      } finally {
        autoAttachBusy = false;
      }
    };

    const unsubTarget = store.sub(playbackTargetAtom, () => {
      if (store.get(playbackTargetAtom).type === "sonos") autoAttachDone = true;
    });
    const unsubPlaying = store.sub(isPlayingAtom, () => {
      if (
        store.get(isPlayingAtom) &&
        store.get(playbackTargetAtom).type === "local"
      ) {
        autoAttachDone = true;
      }
    });
    void tryAttach();
    const unlisten = listen("sonos-topology", () => void tryAttach());
    return () => {
      unsubTarget();
      unsubPlaying();
      unlisten.then((fn) => fn()).catch(() => {});
    };
  }, [store]);

  // Unreachable fallback: the poller reports "lost" after ~30 s; a group
  // that never answered at all gets the same after NO_STATE_TIMEOUT_MS. Go
  // back to This computer without playing, keeping the remembered group.
  useEffect(() => {
    /** The lost state already handled (once per loss). */
    let handled: SonosState | null = null;
    /** Coordinator whose watchdog ran out without any state from it. */
    let silent: string | null = null;
    let watchdog: ReturnType<typeof setTimeout> | null = null;

    const stopWatchdog = () => {
      if (watchdog !== null) clearTimeout(watchdog);
      watchdog = null;
    };

    const fallBack = async (name: string, position: number | null) => {
      store.set(sonosHandoffAtom, { to: "local" });
      try {
        await invoke("notify_track_stopped").catch(() => {});
        await sonosSelectGroup(null, false).catch(warn("deselect"));
        forgetGroupState(store);
        store.set(playbackTargetAtom, { type: "local" });
        const current = keepMirroredQueue(store);
        store.set(isPlayingAtom, false);
        store.set(pendingResumePositionAtom, current ? position : null);
        showToast(`Couldn't reach ${name}`, "error");
      } finally {
        store.set(sonosHandoffAtom, null);
      }
    };

    // Also runs when a handoff ends: "lost" is only sent once, maybe mid-handoff
    const check = () => {
      const target = store.get(playbackTargetAtom);
      const state = store.get(sonosStateAtom);
      if (target.type !== "sonos") return;
      const fromTarget = state?.coordinatorId === target.coordinatorId;
      if (fromTarget) stopWatchdog();
      if (store.get(sonosHandoffAtom)) return;
      const lost =
        fromTarget && state.reachability === "lost" && state !== handled;
      if (!lost && silent !== target.coordinatorId) return;
      if (lost) handled = state;
      silent = null;
      void fallBack(target.name, lost ? state.positionSecs : null);
    };

    // Every selected group must report within the timeout (the poller only
    // reports "lost" for a group it reached at least once).
    const unsubTarget = store.sub(playbackTargetAtom, () => {
      stopWatchdog();
      silent = null;
      const target = store.get(playbackTargetAtom);
      if (target.type !== "sonos") return;
      const coordinatorId = target.coordinatorId;
      if (store.get(sonosStateAtom)?.coordinatorId === coordinatorId) return;
      watchdog = setTimeout(() => {
        watchdog = null;
        silent = coordinatorId;
        check();
      }, NO_STATE_TIMEOUT_MS);
    });
    const unsubState = store.sub(sonosStateAtom, check);
    const unsubHandoff = store.sub(sonosHandoffAtom, check);
    return () => {
      stopWatchdog();
      unsubTarget();
      unsubState();
      unsubHandoff();
    };
  }, [store, showToast]);

  // A resume point belongs to the track it was taken for
  useEffect(
    () =>
      store.sub(currentTrackAtom, () =>
        store.set(pendingResumePositionAtom, null),
      ),
    [store],
  );
}

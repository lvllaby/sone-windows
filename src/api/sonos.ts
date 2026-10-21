import { invoke } from "@tauri-apps/api/core";

// ==================== Types (mirror src-tauri/src/sonos/*) ====================

export interface SonosMember {
  id: string;
  roomName: string;
  ip: string;
}

export interface SonosGroup {
  id: string;
  coordinatorId: string;
  name: string;
  /** Visible members only, coordinator first. */
  members: SonosMember[];
  reachable: boolean;
}

export type QueueItemKind = "tidal" | "tv" | "other";

export interface QueueItem {
  kind: QueueItemKind;
  tidalTrackId: number | null;
  title: string;
  artist: string;
  albumArtUrl: string | null;
  durationSecs: number | null;
}

export type TransportState = "playing" | "paused" | "stopped" | "transitioning";

export type Reachability = "ok" | "reconnecting" | "lost";

export interface MemberVolume {
  memberId: string;
  volume: number;
}

export interface SonosState {
  coordinatorId: string;
  transportState: TransportState;
  /** 0-based queue index; null when not playing from the queue (TV input). */
  trackIndex: number | null;
  positionSecs: number;
  durationSecs: number | null;
  /** 0-100 */
  groupVolume: number;
  muted: boolean;
  memberVolumes: MemberVolume[];
  /** 0 = off, 1 = repeat-all, 2 = repeat-one */
  repeat: number;
  currentItem: QueueItem | null;
  reachability: Reachability;
}

export interface SonosQueue {
  coordinatorId: string;
  updateId: number;
  items: QueueItem[];
}

export interface SonosSnapshot {
  groups: SonosGroup[];
  selected: string | null;
  /** Coordinator of the group containing the remembered last coordinator. */
  attachable: string | null;
  snKnown: boolean;
  manualIp: string | null;
}

/** Error message when the TIDAL-on-Sonos account number can't be found. */
export const SONOS_SN_UNKNOWN = "TIDAL_SN_UNKNOWN";

// ==================== Commands (indices are 0-based) ====================

export async function sonosGetSnapshot(): Promise<SonosSnapshot> {
  return invoke<SonosSnapshot>("sonos_get_snapshot");
}

export async function sonosRescan(): Promise<SonosGroup[]> {
  return invoke<SonosGroup[]>("sonos_rescan");
}

export async function sonosSetManualIp(
  ip: string | null,
): Promise<SonosGroup[]> {
  return invoke<SonosGroup[]>("sonos_set_manual_ip", { ip });
}

export async function sonosSelectGroup(
  coordinatorId: string | null,
  forgetLast: boolean,
): Promise<void> {
  return invoke("sonos_select_group", { coordinatorId, forgetLast });
}

export async function sonosGroupNowPlaying(
  coordinatorId: string,
): Promise<QueueItem | null> {
  return invoke<QueueItem | null>("sonos_group_now_playing", { coordinatorId });
}

export async function sonosLoadQueue(
  trackIds: number[],
  startIndex: number,
  positionSecs: number,
  play: boolean,
): Promise<void> {
  return invoke("sonos_load_queue", {
    trackIds,
    startIndex,
    positionSecs,
    play,
  });
}

export async function sonosPlay(): Promise<void> {
  return invoke("sonos_play");
}

export async function sonosPause(): Promise<void> {
  return invoke("sonos_pause");
}

export async function sonosNext(): Promise<void> {
  return invoke("sonos_next");
}

export async function sonosPrevious(): Promise<void> {
  return invoke("sonos_previous");
}

export async function sonosSeek(positionSecs: number): Promise<void> {
  return invoke("sonos_seek", { positionSecs });
}

export async function sonosPlayIndex(index: number): Promise<void> {
  return invoke("sonos_play_index", { index });
}

export async function sonosQueueInsert(
  trackIds: number[],
  at: number,
): Promise<void> {
  return invoke("sonos_queue_insert", { trackIds, at });
}

export async function sonosQueueRemove(index: number): Promise<void> {
  return invoke("sonos_queue_remove", { index });
}

export async function sonosQueueReplaceAfter(
  index: number,
  trackIds: number[],
): Promise<void> {
  return invoke("sonos_queue_replace_after", { index, trackIds });
}

export async function sonosQueueClear(): Promise<void> {
  return invoke("sonos_queue_clear");
}

/** `volume` is 0-100. */
export async function sonosSetGroupVolume(volume: number): Promise<void> {
  return invoke("sonos_set_group_volume", { volume });
}

export async function sonosSetMemberVolume(
  memberId: string,
  volume: number,
): Promise<void> {
  return invoke("sonos_set_member_volume", { memberId, volume });
}

export async function sonosSetMute(muted: boolean): Promise<void> {
  return invoke("sonos_set_mute", { muted });
}

/** `repeat`: 0 = off, 1 = repeat-all, 2 = repeat-one. */
export async function sonosSetPlayMode(repeat: number): Promise<void> {
  return invoke("sonos_set_play_mode", { repeat });
}

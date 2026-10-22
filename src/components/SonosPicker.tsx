import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { ReactNode } from "react";
import { useAtom, useAtomValue } from "jotai";
import { Check, Loader2, Monitor, Speaker, Volume2 } from "lucide-react";
import MenuPortal from "./MenuPortal";
import { playbackTargetAtom } from "../atoms/playback";
import {
  sonosGroupVolumeAtom,
  sonosGroupsAtom,
  sonosHandoffAtom,
  sonosPickerAtom,
  sonosStateAtom,
} from "../atoms/sonos";
import {
  sonosGroupNowPlaying,
  sonosSetMemberVolume,
  type SonosGroup,
} from "../api/sonos";
import { useHeldValue } from "../hooks/useHeldValue";
import { usePlaybackActions } from "../hooks/usePlaybackActions";
import { useSonosHandoff } from "../hooks/useSonosHandoff";

const MEMBER_VOLUME_THROTTLE_MS = 100;
const PICKER_WIDTH = 288;

/** A thin slider; `value` is 0-1. */
function PickerSlider({
  value,
  onChange,
  onGrab,
  label,
}: {
  value: number;
  onChange: (value: number) => void;
  onGrab: () => void;
  label: string;
}) {
  return (
    <div className="flex-1 relative h-4 flex items-center group/vol">
      <input
        type="range"
        min="0"
        max="1"
        step="0.01"
        value={value}
        aria-label={label}
        onChange={(e) => onChange(parseFloat(e.target.value))}
        onPointerDown={onGrab}
        className="absolute inset-0 w-full h-full opacity-0 z-10 cursor-pointer"
      />
      <div className="relative w-full h-[3px] group-hover/vol:h-[4px] transition-[height] duration-100 rounded-full">
        <div className="absolute inset-0 bg-th-slider-track rounded-full" />
        <div
          className="absolute h-full bg-th-slider-fill group-hover/vol:bg-th-accent rounded-full transition-colors duration-100"
          style={{ width: `${value * 100}%` }}
        />
        <div
          className="absolute top-1/2 -translate-y-1/2 w-[10px] h-[10px] bg-th-text-primary rounded-full shadow-sm opacity-0 group-hover/vol:opacity-100 transition-opacity duration-100"
          style={{ left: `calc(${value * 100}% - 5px)` }}
        />
      </div>
    </div>
  );
}

function GroupVolume() {
  const groupVolume = useAtomValue(sonosGroupVolumeAtom);
  const { setVolume } = usePlaybackActions();
  const held = useHeldValue(groupVolume);

  return (
    <div className="flex items-center gap-2">
      <Volume2 size={14} className="text-th-text-muted shrink-0" />
      <PickerSlider
        value={held.value}
        label="Group volume"
        onGrab={held.grab}
        onChange={(v) => {
          held.change(v);
          void setVolume(v);
        }}
      />
    </div>
  );
}

function MemberVolume({
  memberId,
  roomName,
  volume,
  send,
}: {
  memberId: string;
  roomName: string;
  /** 0-100, as polled */
  volume: number;
  send: (memberId: string, volume: number) => void;
}) {
  const held = useHeldValue(volume / 100);

  return (
    <div className="flex items-center gap-2">
      <span className="w-20 shrink-0 truncate text-[11px] text-th-text-muted">
        {roomName}
      </span>
      <PickerSlider
        value={held.value}
        label={`${roomName} volume`}
        onGrab={held.grab}
        onChange={(v) => {
          held.change(v);
          send(memberId, Math.round(v * 100));
        }}
      />
    </div>
  );
}

/** One request per 100 ms per member; the last value always goes out. */
function useMemberVolumeSender() {
  const timers = useRef(new Map<string, ReturnType<typeof setTimeout>>());
  const pending = useRef(new Map<string, number>());

  // Closing mid-drag still sends the last value
  useEffect(() => {
    const t = timers.current;
    const p = pending.current;
    return () => {
      t.forEach((id) => clearTimeout(id));
      p.forEach((value, memberId) =>
        sonosSetMemberVolume(memberId, value).catch(() => {}),
      );
      p.clear();
    };
  }, []);

  return (memberId: string, volume: number) => {
    pending.current.set(memberId, volume);
    if (timers.current.has(memberId)) return;
    const flush = () => {
      timers.current.delete(memberId);
      const value = pending.current.get(memberId);
      if (value === undefined) return;
      pending.current.delete(memberId);
      sonosSetMemberVolume(memberId, value).catch((e) =>
        console.warn("[sonos] member volume failed:", e),
      );
      timers.current.set(
        memberId,
        setTimeout(flush, MEMBER_VOLUME_THROTTLE_MS),
      );
    };
    flush();
  };
}

/** `<rooms> · <artist> – <title>` for multi-room groups, else `<artist> – <title>` (just the title on TV). */
function nowPlayingSubtitle(
  group: SonosGroup,
  item: { title: string; artist: string },
): string {
  const what = item.artist ? `${item.artist} – ${item.title}` : item.title;
  // A single room would only repeat the row title
  if (group.members.length <= 1) return what;
  return `${group.members.map((m) => m.roomName).join(", ")} · ${what}`;
}

function Row({
  icon,
  title,
  subtitle,
  active,
  busy,
  disabled,
  dim,
  onClick,
  autoFocus,
}: {
  icon: ReactNode;
  title: string;
  subtitle?: string;
  active: boolean;
  busy: boolean;
  disabled: boolean;
  dim?: boolean;
  onClick: () => void;
  autoFocus?: boolean;
}) {
  return (
    <button
      onClick={onClick}
      disabled={disabled}
      autoFocus={autoFocus}
      className={`w-full flex items-center gap-3 px-3 py-2 text-left transition-colors ${
        dim ? "opacity-50" : ""
      } ${disabled ? "cursor-not-allowed" : "hover:bg-th-border-subtle"}`}
    >
      <span
        className={`shrink-0 ${active ? "text-th-accent" : "text-th-text-muted"}`}
      >
        {icon}
      </span>
      <span className="min-w-0 flex-1">
        <span
          className={`block text-[13px] truncate ${
            active ? "text-th-accent" : "text-th-text-primary"
          }`}
        >
          {title}
        </span>
        {subtitle && (
          <span className="block text-[11px] text-th-text-muted truncate">
            {subtitle}
          </span>
        )}
      </span>
      {busy ? (
        <Loader2
          size={14}
          className="shrink-0 animate-spin text-th-text-muted"
        />
      ) : (
        active && <Check size={14} className="shrink-0 text-th-accent" />
      )}
    </button>
  );
}

/** Mounted once in Layout; renders only while open. */
export default function SonosPicker() {
  const [picker, setPicker] = useAtom(sonosPickerAtom);
  if (!picker) return null;
  const { opener } = picker;
  // Escape puts focus back on the opener, if it's still there
  const close = (returnFocus: boolean) => {
    setPicker(null);
    if (returnFocus && opener.isConnected) opener.focus();
  };
  return <PickerPanel opener={opener} onClose={close} />;
}

function PickerPanel({
  opener,
  onClose,
}: {
  opener: HTMLElement;
  onClose: (returnFocus: boolean) => void;
}) {
  const groups = useAtomValue(sonosGroupsAtom);
  const target = useAtomValue(playbackTargetAtom);
  const state = useAtomValue(sonosStateAtom);
  const handoff = useAtomValue(sonosHandoffAtom);
  const { switchTarget, handoffInProgress } = useSonosHandoff();
  const panelRef = useRef<HTMLDivElement>(null);
  const sendMemberVolume = useMemberVolumeSender();
  const [pos, setPos] = useState<{ bottom: number; left: number } | null>(null);
  const [subtitles, setSubtitles] = useState<Record<string, string>>({});

  // Anchored above the opener, right edges aligned, kept inside the window;
  // bottom-right when the opener is gone or hidden
  useLayoutEffect(() => {
    const rect = opener.isConnected ? opener.getBoundingClientRect() : null;
    if (!rect || (rect.width === 0 && rect.height === 0)) {
      setPos({ bottom: 100, left: window.innerWidth - PICKER_WIDTH - 16 });
      return;
    }
    const left = Math.max(
      8,
      Math.min(rect.right - PICKER_WIDTH, window.innerWidth - PICKER_WIDTH - 8),
    );
    setPos({ bottom: window.innerHeight - rect.top + 8, left });
  }, [opener]);

  // Now-playing subtitles: fetched once per open, in parallel, never polled
  useEffect(() => {
    let cancelled = false;
    for (const group of groups) {
      if (!group.reachable) continue;
      sonosGroupNowPlaying(group.coordinatorId)
        .then((item) => {
          if (cancelled || !item) return;
          const text = nowPlayingSubtitle(group, item);
          setSubtitles((prev) => ({ ...prev, [group.coordinatorId]: text }));
        })
        .catch(() => {});
    }
    return () => {
      cancelled = true;
    };
    // Once per open: the group list may change while open, the subtitles don't
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const onMouseDown = (e: MouseEvent) => {
      const el = e.target as HTMLElement;
      if (panelRef.current?.contains(el)) return;
      // The openers toggle the picker themselves
      if (el.closest("[data-sonos-trigger]")) return;
      onClose(false);
    };
    // Capture phase: Escape closes only the picker, not a player or modal below
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.preventDefault();
      e.stopImmediatePropagation();
      onClose(true);
    };
    document.addEventListener("mousedown", onMouseDown);
    window.addEventListener("keydown", onKeyDown, true);
    return () => {
      document.removeEventListener("mousedown", onMouseDown);
      window.removeEventListener("keydown", onKeyDown, true);
    };
  }, [onClose]);

  const activeGroup =
    target.type === "sonos"
      ? groups.find((g) => g.coordinatorId === target.coordinatorId)
      : undefined;
  const memberVolumes =
    state?.coordinatorId === activeGroup?.coordinatorId
      ? state?.memberVolumes
      : undefined;

  return (
    <MenuPortal>
      <div
        ref={panelRef}
        role="dialog"
        aria-label="Play on"
        style={{
          position: "fixed",
          width: PICKER_WIDTH,
          bottom: pos?.bottom ?? 0,
          left: pos?.left ?? 0,
          visibility: pos ? "visible" : "hidden",
        }}
        className="z-[10000] bg-th-surface rounded-lg shadow-2xl shadow-black/60 border border-th-border-subtle py-1 animate-fadeIn select-none"
      >
        <div className="px-3 pt-2 pb-1 text-[11px] uppercase tracking-wider text-th-text-muted">
          Play on
        </div>
        <Row
          icon={<Monitor size={16} />}
          title="This computer"
          active={target.type === "local"}
          busy={handoff?.to === "local"}
          disabled={handoffInProgress}
          onClick={() => void switchTarget("local")}
          autoFocus
        />
        {groups.map((group) => {
          const active =
            target.type === "sonos" &&
            target.coordinatorId === group.coordinatorId;
          return (
            <Row
              key={group.id}
              icon={<Speaker size={16} />}
              title={group.name}
              subtitle={subtitles[group.coordinatorId]}
              active={active}
              busy={handoff?.to === group.coordinatorId}
              disabled={handoffInProgress || (!group.reachable && !active)}
              dim={!group.reachable}
              onClick={() => void switchTarget(group.coordinatorId)}
            />
          );
        })}
        {activeGroup && (
          <div className="px-3 pt-2 pb-2 mt-1 border-t border-th-border-subtle space-y-2">
            <GroupVolume />
            {activeGroup.members.length > 1 &&
              activeGroup.members.map((m) => (
                <MemberVolume
                  key={m.id}
                  memberId={m.id}
                  roomName={m.roomName}
                  volume={
                    memberVolumes?.find((v) => v.memberId === m.id)?.volume ?? 0
                  }
                  send={sendMemberVolume}
                />
              ))}
          </div>
        )}
      </div>
    </MenuPortal>
  );
}

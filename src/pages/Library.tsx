import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { localMediaUrl, useDownloadJobs, type DownloadJob } from "../download";
import { IBack, IClose, IDownload, IPlayF } from "../icons";

function bytesOf(n: number) {
  if (!n) return "0 B";
  if (n < 1024 * 1024) return `${Math.max(1, Math.round(n / 1024))} KB`;
  return `${(n / (1024 * 1024)).toFixed(n >= 10 * 1024 * 1024 ? 0 : 1)} MB`;
}

function groupKey(job: DownloadJob) {
  return job.video_id && job.video_id > 0 ? `v${job.video_id}` : `t${job.title || "未命名"}`;
}

export default function LibraryPage({ onBack }: { onBack: () => void }) {
  const jobs = useDownloadJobs();
  const [playing, setPlaying] = useState<DownloadJob | null>(null);
  const [confirmId, setConfirmId] = useState<number | null>(null);
  const visible = jobs;
  const groups = useMemo(() => {
    const map = new Map<string, DownloadJob[]>();
    for (const job of visible) {
      const key = groupKey(job);
      map.set(key, [...(map.get(key) || []), job]);
    }
    return [...map.entries()].map(([key, items]) => ({
      key,
      title: items[0]?.title || "未命名",
      items: [...items].sort((a, b) => a.episode - b.episode || a.id - b.id),
    }));
  }, [visible]);
  const stored = visible.filter((job) => job.phase === "done").reduce((sum, job) => sum + (job.bytes || 0), 0);
  const active = visible.filter((job) => job.phase !== "done" && job.phase !== "cancelled");

  return (
    <div className="absolute inset-0 bg-bg flex flex-col">
      <div className="shrink-0 px-3 pb-2 flex items-center gap-2" style={{ paddingTop: "calc(var(--sat, 0px) + 10px)" }}>
        <button onClick={onBack} aria-label="返回" className="w-9 h-9 rounded-full bg-s3 flex items-center justify-center text-on pressable"><IBack size={18} /></button>
        <div className="min-w-0">
          <div className="text-[18px] font-bold text-on leading-none">离线视频</div>
          <div className="text-[10.5px] text-mut mt-1">已保存 {bytesOf(stored)} · 只在你点删除时才会清掉</div>
        </div>
      </div>
      <div className="flex-1 scroll-y px-4 pb-24">
        {active.length > 0 && (
          <div className="rounded-(--r-md) bg-s2 p-3 mb-4">
            <div className="text-[12px] font-semibold text-on mb-2">下载队列</div>
            {active.map((job) => (
              <div key={job.id} className="py-2 border-t border-white/5 first:border-0">
                <div className="flex items-center gap-2">
                  <IDownload size={14} className="text-mut shrink-0" />
                  <div className="min-w-0 flex-1">
                    <div className="truncate text-[12.5px] text-on">{job.title || "视频"}{job.episode > 0 ? ` · 第${job.episode}集` : ""}</div>
                    <div className="text-[10.5px] text-mut truncate">{job.message || job.phase}{job.total > 0 ? ` · ${bytesOf(job.received)} / ${bytesOf(job.total)}` : ""}</div>
                  </div>
                  {(job.phase === "running" || job.phase === "queued") && <button className="text-[11px] text-mut pressable" onClick={() => invoke("download_pause", { id: job.id }).catch(() => {})}>暂停</button>}
                  {(job.phase === "paused" || job.phase === "failed") && <button className="text-[11px] font-semibold text-primary pressable" onClick={() => invoke("download_resume", { id: job.id }).catch(() => {})}>继续</button>}
                </div>
              </div>
            ))}
            <div className="text-[10px] text-dim mt-1">进程被系统强制结束后，这里会停住。点继续才会从断点接着下，不能保证在后台一直跑。</div>
          </div>
        )}
        {!groups.length && <div className="text-center text-[12.5px] text-mut py-16">还没有离线视频。在播放页可以下载本集或全集。</div>}
        {groups.map((group) => (
          <section key={group.key} className="mb-4">
            <div className="text-[14px] font-bold text-on mb-2">{group.title}</div>
            <div className="rounded-(--r-md) bg-s1 overflow-hidden">
              {group.items.map((job) => (
                <div key={job.id} className="flex items-center gap-2 px-3 py-2.5 border-t border-white/5 first:border-0">
                  <button className="min-w-0 flex-1 text-left pressable" disabled={job.phase !== "done"} onClick={() => job.phase === "done" && setPlaying(job)}>
                    <div className="text-[12.5px] text-on">{job.episode > 0 ? `第 ${job.episode} 集` : "单集"}{job.phase === "done" && job.position && job.position > 3 ? ` · 看到 ${Math.floor(job.position)} 秒` : ""}</div>
                    <div className="text-[10.5px] text-mut truncate">{job.phase === "done" ? bytesOf(job.bytes) : (job.message || job.phase)}</div>
                  </button>
                  {job.phase === "done" && <button aria-label="播放" className="w-8 h-8 rounded-full bg-primary text-on-primary flex items-center justify-center pressable" onClick={() => setPlaying(job)}><IPlayF size={14} /></button>}
                  <button className="text-[11px] text-mut pressable" onClick={() => {
                    if (confirmId === job.id) invoke("download_delete", { id: job.id }).finally(() => setConfirmId(null));
                    else setConfirmId(job.id);
                  }}>{confirmId === job.id ? "确认删除" : "删除"}</button>
                  {confirmId === job.id && <button aria-label="取消删除" className="text-mut pressable" onClick={() => setConfirmId(null)}><IClose size={12} /></button>}
                </div>
              ))}
            </div>
          </section>
        ))}
      </div>
      {playing && (
        <LocalPlayer
          job={playing}
          next={jobs.filter((item) => item.phase === "done" && groupKey(item) === groupKey(playing) && item.episode > playing.episode).sort((a, b) => a.episode - b.episode)[0]}
          onClose={() => setPlaying(null)}
          onNext={(job) => setPlaying(job)}
        />
      )}
    </div>
  );
}

function LocalPlayer({ job, next, onClose, onNext }: { job: DownloadJob; next?: DownloadJob; onClose: () => void; onNext: (job: DownloadJob) => void }) {
  const ref = useRef<HTMLVideoElement>(null);
  const saved = useRef(false);
  useEffect(() => { saved.current = false; }, [job.id]);
  const remember = (position: number, duration: number) => {
    invoke("download_progress", { id: job.id, position, duration }).catch(() => {});
  };
  return (
    <div className="absolute inset-0 z-40 bg-black flex flex-col">
      <div className="shrink-0 flex items-center gap-2 px-3 text-white" style={{ paddingTop: "calc(var(--sat, 0px) + 8px)" }}>
        <button onClick={onClose} aria-label="关闭播放" className="w-9 h-9 flex items-center justify-center pressable"><IBack size={18} /></button>
        <div className="min-w-0 truncate text-[13px]">{job.title}{job.episode > 0 ? ` · 第${job.episode}集` : ""}</div>
      </div>
      <video
        key={job.id}
        ref={ref}
        src={localMediaUrl(job.id)}
        controls
        autoPlay
        playsInline
        className="flex-1 w-full bg-black"
        onLoadedMetadata={(e) => {
          const video = e.currentTarget;
          if (!saved.current && (job.position || 0) > 1 && video.duration && job.position! < video.duration - 1) {
            video.currentTime = job.position || 0;
            saved.current = true;
          }
        }}
        onTimeUpdate={(e) => {
          const video = e.currentTarget;
          if (Math.floor(video.currentTime) % 2 === 0) remember(video.currentTime, video.duration || job.duration || 0);
        }}
        onPause={(e) => remember(e.currentTarget.currentTime, e.currentTarget.duration || 0)}
        onEnded={() => { if (next) onNext(next); }}
      />
      <div className="shrink-0 flex items-center justify-between px-4 py-3 text-white" style={{ paddingBottom: "calc(var(--sab, 0px) + 12px)" }}>
        <span className="text-[11px] text-white/70">本地播放，不需要网络</span>
        <button disabled={!next} className="h-8 px-3 rounded-full bg-white/15 text-[12px] font-semibold disabled:opacity-40 pressable" onClick={() => next && onNext(next)}>
          {next ? `下一集 · 第${next.episode}集` : "没有下一集"}
        </button>
      </div>
    </div>
  );
}

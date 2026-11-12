import { invoke } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";
import { IClose, IDownload } from "./icons";
import { useStore } from "./store";

export type DownloadJob = {
  id: number;
  title: string;
  url: string;
  episode: number;
  phase: "running" | "done" | "failed" | "cancelled" | string;
  received: number;
  total: number;
  index: number;
  count: number;
  message: string;
  path: string;
  token: string;
  bytes: number;
};

const ARMED = "hghub-download-active";

function armed() {
  try { return sessionStorage.getItem(ARMED) === "1"; } catch { return false; }
}

export async function startDownload(url: string, title: string, episode = 0) {
  try { sessionStorage.setItem(ARMED, "1"); } catch { /* 隐私模式不影响下载本身 */ }
  window.dispatchEvent(new Event("hg:download"));
  return invoke<DownloadJob>("download_start", { url, title, episode });
}

export function useDownloadJobs() {
  const [jobs, setJobs] = useState<DownloadJob[]>([]);
  const [on, setOn] = useState(armed);
  useEffect(() => {
    const wake = () => setOn(true);
    window.addEventListener("hg:download", wake);
    return () => window.removeEventListener("hg:download", wake);
  }, []);
  useEffect(() => {
    if (!on) return;
    let stop = false;
    const tick = () => {
      invoke<DownloadJob[]>("download_list")
        .then((list) => { if (!stop) setJobs(list); })
        .catch(() => { /* 浏览器预览没有原生命令 */ });
    };
    tick();
    const timer = window.setInterval(tick, 700);
    return () => { stop = true; window.clearInterval(timer); };
  }, [on]);
  return jobs;
}

function percent(job: DownloadJob) {
  if (job.phase === "done") return 100;
  if (job.count > 0) return Math.min(99, Math.round((job.index / job.count) * 100));
  return null;
}

export function DownloadBanner() {
  const jobs = useDownloadJobs();
  const { toast } = useStore();
  const [hidden, setHidden] = useState<number[]>([]);
  const job = [...jobs].reverse().find((item) => !hidden.includes(item.id));
  if (!job) return null;
  const pct = percent(job);
  const label = job.episode > 0 ? `${job.title || "视频"} · 第${job.episode}集` : (job.title || "视频");
  return (
    <div className="absolute left-3 right-3 z-[70] rounded-2xl px-3 py-2.5 text-white shadow-lg"
      style={{ top: "112px", background: "rgba(16,18,24,.92)" }}>
      <div className="flex items-center gap-2">
        <IDownload size={16} />
        <div className="min-w-0 flex-1">
          <div className="truncate text-[12px] font-semibold">{label}</div>
          <div className="truncate text-[10.5px] text-white/70">
            {job.phase === "running" ? (pct == null ? "正在准备。退出应用会中断，不支持断点续传" : `正在下载 ${pct}%。切页面会继续，退出应用会中断`) : job.message}
          </div>
        </div>
        {job.phase === "running" && (
          <button className="h-7 px-2 text-[11px] text-white/80" onClick={() => invoke("download_cancel", { id: job.id }).catch(() => {})}>取消</button>
        )}
        {job.phase === "done" && (
          <button className="h-7 px-2 text-[11px] font-semibold text-amber-200" onClick={() => invoke("download_open", { id: job.id }).catch((e) => toast((e as Error).message || "无法打开", "info"))}>打开</button>
        )}
        {job.phase !== "running" && (
          <button aria-label="关闭下载提示" className="h-7 w-7 flex items-center justify-center" onClick={() => setHidden((ids) => [...ids, job.id])}>
            <IClose size={14} />
          </button>
        )}
      </div>
      {job.phase === "running" && (
        <div className="mt-2 h-1 overflow-hidden rounded-full bg-white/15">
          <div className="h-full bg-white/80" style={{ width: `${pct ?? 8}%` }} />
        </div>
      )}
    </div>
  );
}

export function DownloadButton({
  url, title, episode = 0, variant,
}: { url: string; title: string; episode?: number; variant: "rail" | "row" }) {
  const jobs = useDownloadJobs();
  const { toast } = useStore();
  const mine = [...jobs].reverse().find((job) => job.url === url);
  const running = jobs.some((job) => job.phase === "running");
  const pct = mine && mine.phase === "running" ? percent(mine) : null;
  const start = () => {
    if (!url) return;
    if (mine?.phase === "running") {
      invoke("download_cancel", { id: mine.id }).catch(() => {});
      return;
    }
    if (mine?.phase === "done") {
      invoke("download_open", { id: mine.id }).catch(() => toast("没有可打开该视频的播放器", "info"));
      return;
    }
    startDownload(url, title, episode).catch((e) => {
      toast((e as Error).message || "下载失败", "info");
    });
  };
  if (variant === "row") {
    const text = !url ? "正在获取片源" : mine?.phase === "running" ? `下载中 ${pct ?? 0}% · 点此取消` : mine?.phase === "done" ? "已下载，点此打开" : `下载${episode > 0 ? `第${episode}集` : "本集"}`;
    return (
      <button type="button" onClick={() => { if (mine?.phase === "done") invoke("download_open", { id: mine.id }).catch(() => {}); else start(); }}
        disabled={!url || (running && mine?.phase !== "running" && mine?.phase !== "done")}
        className="mt-3 flex h-10 w-full items-center justify-center gap-1.5 rounded-full bg-primary text-[12.5px] font-semibold text-on-primary pressable disabled:opacity-50">
        <IDownload size={15} />{text}
      </button>
    );
  }
  const caption = mine?.phase === "running" ? `${pct ?? 0}%` : mine?.phase === "done" ? "已存" : mine?.phase === "failed" ? "失败" : "下载";
  return (
    <button type="button" onClick={start} disabled={!url} aria-label="下载本集"
      className="flex flex-col items-center gap-1 pressable disabled:opacity-50">
      <IDownload size={25} className="hg-ink hg-ov-shadow" />
      <span className="text-[10.5px] hg-ink-2 hg-ov-shadow">{caption}</span>
    </button>
  );
}

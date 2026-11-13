import { invoke } from "@tauri-apps/api/core";
import { tracePlayback } from "./playbackDiagnostics";
import type {
  CommentPage, Detail, HotWord, Paged, PlayInfo, Post, PostCategory, RankItem, TagGroup, TopicCard, TopicDetail, User, UserProfile, VideoCard,
} from "./types";

/* ── 运行环境 ───────────────────────────────────────────── */

/** 应用版本（构建时由 vite.config.ts 从 package.json 注入） */
export const APP_VERSION = __APP_VERSION__;

const ua = navigator.userAgent;
export const isAndroid = /Android/i.test(ua);
/** iPhone 的 UA 也带「like Mac OS X」，必须先于 macOS 判断；iPadOS 默认伪装成 Mac，用触点数区分。 */
export const isIos =
  /iPhone|iPad|iPod/i.test(ua) ||
  (/Macintosh/i.test(ua) && navigator.maxTouchPoints > 1) ||
  // iPadOS 13+ 默认把自己报成 MacIntel，只能靠触点数认
  (typeof navigator.platform === "string" && navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1);
export const isMacos = !isIos && /Macintosh|Mac OS X/i.test(ua);
/** Linux 桌面：webkit2gtk 的 UA 是「X11; Linux x86_64」。必须排在 Windows 之前判断，
 *  否则会被当成 Windows；而 Android 的 UA 里同样带「Linux」，所以必须先排除安卓。 */
export const isLinux = !isAndroid && !isIos && !isMacos && /Linux|X11/i.test(ua);
export const platform: "android" | "ios" | "windows" | "macos" | "linux" =
  isAndroid ? "android" : isIos ? "ios" : isMacos ? "macos" : isLinux ? "linux" : "windows";
/** WebView 内核版本：能不能走 MSE、能解哪些封装，是「无法播放」的首要判据。
 *  安卓取 Chrome/xxx（System WebView），Linux/macOS 取 WebKit 的 Version/xxx。 */
export const webviewVersion =
  /Chrome\/([\d.]+)/.exec(ua)?.[1] || /Version\/([\d.]+)/.exec(ua)?.[1] || "?";
/** 移动端（安卓 / iOS）共用触屏界面 */
export const isMobile = isAndroid || isIos;

/** 媒体基址随平台不同（Rust 侧对应 `media::LOCAL_ORIGIN`，两边必须一致）：
 *  Windows / Android → `http://<scheme>.localhost`（自定义协议，WebView 自己加载）
 *  macOS / iOS → `hg://localhost`（WebKit 的 scheme handler）
 *  Linux → `http://127.0.0.1:17650`：WebKitGTK 的 <video> 由 GStreamer 播放，
 *          它拿不到自定义 scheme，所以由 Rust 侧在 loopback 上起 HTTP 代理
 *          （`src-tauri/src/linux_http.rs`，端口必须与此一致）。 */
export const LOCAL =
  isMacos || isIos ? "hg://localhost" : isLinux ? "http://127.0.0.1:17650" : "http://hg.localhost";

/** src 是否已是本地协议地址（避免在详情页把代理地址再包一层） */
export const isLocalMedia = (src: string) => src.startsWith(`${LOCAL}/`);

/** 封面：经本地协议解密（站点封面是 AES 密文）。 */
export const cover = (url?: string, _w = 480) =>
  url ? `${LOCAL}/img?u=${encodeURIComponent(url)}` : "";

/** 播放列表：经本地协议改写分片地址，规避 CORS 与防盗链。 */
export const playlist = (url: string) => `${LOCAL}/hls?u=${encodeURIComponent(url)}`;

/* ── 调用封装 ───────────────────────────────────────────── */

/** 写一行到应用数据目录 debug.log（用户反馈「打不开」时要的就是它） */
export const logLine = (msg: string) => invoke("debug_log", { msg }).catch(() => {});

/** 网络类失败的样子：没有 HTTP 状态码，纯链路层（DNS / 连接 / 超时 / TLS） */
const NET_FAIL = /dns|timed? ?out|timeout|connection|refused|reset|unreachable|broken pipe|tls|ssl|handshake|sending request|error sending|证书|网络|连接|超时|中断/i;
/** 只读命令：链路抖动时可以安全重试一次；写操作绝不重试（避免重复提交） */
const READ_ONLY = new Set([
  "api_me", "api_feed", "api_category", "api_rank", "api_search",
  "api_posts", "api_post", "api_post_comments", "api_video", "api_play", "api_comments",
  "api_topics", "api_topic", "api_tags", "api_tag_videos", "api_search_hot",
  "api_post_playback", "api_post_categories", "api_post_category", "api_related", "api_following", "api_user", "api_user_stats", "api_user_videos",
]);
/** 给自动选线用的观察点。api.ts 不反向依赖 autoLine，避免循环 import。 */
export type NetObserver = (r: { ok: boolean; cmd: string; msg?: string; networkish?: boolean }) => void;
let netObserver: NetObserver | null = null;
export const setNetObserver = (fn: NetObserver | null) => { netObserver = fn; };
const notifyNet = (r: Parameters<NetObserver>[0]) => {
  try { netObserver?.(r); } catch { /* 观察者不能影响请求本身 */ }
};
const failMessage = (msg: string, status?: string) =>
  status
    ? `服务器请求失败（HTTP ${status}），请稍后重试或在设置中切换线路`
    : msg.length > 140 ? `${msg.slice(0, 140)}…` : msg;

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const once = () => invoke(cmd, args) as Promise<T>;
  try {
    const r = await once();
    notifyNet({ ok: true, cmd });
    return r;
  } catch (e) {
    const msg = typeof e === "string" ? e : "请求失败，请检查网络";
    const status = /^HTTP (\d{3})\b/.exec(msg)?.[1];
    const networkish = !status && NET_FAIL.test(msg);
    // 落盘完整错误链（dns error / connection refused / 证书错误…），toast 只显示截断版
    logLine(`[api] ${cmd} → ${msg}`);
    tracePlayback("api.fail", { cmd, err: msg, status: status ?? null, networkish });
    notifyNet({ ok: false, cmd, msg, networkish });
    // 线路可能正被自动切走：给切换留一点时间，只读请求重试一次，用户就不用看见这条错误
    if (networkish && READ_ONLY.has(cmd)) {
      logLine(`[api] ${cmd} 链路异常，1.2s 后重试一次`);
      tracePlayback("api.retry", { cmd, err: msg });
      await new Promise((r) => setTimeout(r, 1200));
      try {
        const r = await once();
        logLine(`[api] ${cmd} 重试成功`);
        tracePlayback("api.retry_ok", { cmd });
        notifyNet({ ok: true, cmd });
        return r;
      } catch (e2) {
        const again = typeof e2 === "string" ? e2 : msg;
        const againStatus = /^HTTP (\d{3})\b/.exec(again)?.[1];
        logLine(`[api] ${cmd} 重试仍失败 → ${again}`);
        tracePlayback("api.retry_failed", { cmd, err: again, status: againStatus ?? null });
        throw new Error(failMessage(again, againStatus));
      }
    }
    throw new Error(failMessage(msg, status));
  }
}

/* ── 接口 ───────────────────────────────────────────────── */

export const api = {
  boot: () =>
    call<{
      user: User | null;
      logged_in: boolean;
      base: string;
      strict_security?: boolean;
      cli_relaxed?: boolean;
    }>("app_boot"),
  securitySet: (strict: boolean) => call<boolean>("security_set", { strict }),
  login: (username: string, password: string) =>
    call<{ user: User }>("api_login", { username, password }),
  register: (username: string, password: string, email = "") =>
    call<{ user: User }>("api_register", { username, password, email }),
  logout: () => call<{ ok: boolean }>("api_logout"),
  me: () => call<User>("api_me"),

  feed: (page = 1, size = 20) => call<Paged<VideoCard>>("api_feed", { page, size }),
  topics: () => call<{ items: TopicCard[] }>("api_topics"),
  topic: (slug: string) => call<TopicDetail>("api_topic", { slug }),
  tags: () => call<{ categories: TagGroup[] }>("api_tags"),
  tagVideos: (tag: string, page = 1, size = 20) =>
    call<Paged<VideoCard>>("api_tag_videos", { tag, page, size }),
  category: (cate: string, page = 1, size = 20) =>
    call<Paged<VideoCard>>("api_category", { cate, page, size }),
  rank: (kind: "hot" | "recommend" | "potential") =>
    call<{ type: string; items: RankItem[] }>("api_rank", { kind }),
  /** 全站搜索。`kind` 省略时后端只搜短剧；吃瓜帖要显式传 "post"（issue #34）。 */
  search: (q: string, page = 1, size = 20, kind?: "video" | "post") =>
    call<{ items: VideoCard[]; pagination: Paged<VideoCard>["pagination"]; keyword: string }>(
      "api_search", { q, page, size, kind: kind ?? null },
    ),
  /** 只搜吃瓜帖：返回结构是 Post 而不是 VideoCard。 */
  searchPosts: (q: string, page = 1, size = 20) =>
    call<{ items: Post[]; pagination: Paged<Post>["pagination"]; keyword: string }>(
      "api_search", { q, page, size, kind: "post" },
    ),
  searchHot: () => call<{ items: HotWord[] }>("api_search_hot"),
  posts: (page = 1, size = 20) => call<Paged<Post>>("api_posts", { page, size }),
  post: (id: number) => call<Post & { content?: string; video_url?: string }>("api_post", { id }),
  /** 吃瓜帖视频：详情里 video_url 恒为空，地址由 playback 接口下发（可能多段）。 */
  postPlayback: (id: number) => call<{ mode?: number; videos?: { index: number; url: string }[] }>("api_post_playback", { id }),

  video: (id: number) => call<Detail>("api_video", { id }),
  play: (id: number, ep: number) => call<PlayInfo>("api_play", { id, ep }),

  comments: (id: number, ep: number, page = 1, size = 20) =>
    call<CommentPage>("api_comments", { id, ep, page, size }),
  commentSend: (id: number, ep: number, text: string, replyId?: number) =>
    call<unknown>("api_comment_send", { id, ep, text, replyId: replyId ?? null }),

  clearCache: () => invoke<void>("cache_clear"),
  netDiag: () => invoke<Record<string, unknown>>("net_diag"),
  postLike: (id: number, on: boolean) => invoke<unknown>("api_post_like", { id, on }),
  postCollect: (id: number, on: boolean) => invoke<unknown>("api_post_collect", { id, on }),
  postComments: (id: number, page: number, size: number) => call<CommentPage>("api_post_comments", { id, page, size }),
  postCategories: () => call<{ items: PostCategory[] }>("api_post_categories"),
  postCategory: (route: string, page = 1, size = 20) =>
    call<Paged<Post>>("api_post_category", { route, page, size }),
  related: (id: number) => call<{ items: VideoCard[] }>("api_related", { id }),
  following: (page = 1, size = 20) => call<Paged<VideoCard>>("api_following", { page, size }),
  user: (id: number) => call<UserProfile>("api_user", { id }),
  userStats: (id: number) => call<{ user_id: number; like_count: number; following: number; followers: number; video_count: number; post_count: number }>("api_user_stats", { id }),
  userVideos: (id: number, page = 1, size = 20) => call<Paged<VideoCard>>("api_user_videos", { id, page, size }),
  like: (id: number, ep: number, on: boolean) => call<unknown>("api_like", { id, ep, on }),
  collect: (id: number, on: boolean) => call<unknown>("api_collect", { id, on }),
  follow: (uid: number, on: boolean) => call<unknown>("api_follow", { uid, on }),

  /* 线路（数据源）：固定线路写死在 Rust 侧，镜像线路从地址发布页拉、推荐镜像从发布入口页拉 */
  lineState: () => call<LineState>("line_state"),
  lineSet: (url: string) => call<string>("line_set", { url }),
  lineMirrors: () => call<LineMirrors>("line_mirrors"),
  lineProbe: (urls: string[]) => call<LineProbe[]>("line_probe", { urls }),
  lineAuto: () => call<LineAuto>("line_auto"),
  /* 当前是镜像时敲一次主站，通了就切回（不清会话） */
  lineTryMain: () => call<LineTryMain>("line_try_main"),
  /* 线路体检：探一次 /api/videos，和自动选线同一判据 */
  lineHealth: () => call<LineHealth>("line_health"),

  /* 更新检查：对比 GitHub Release 的 tag */
  checkUpdate: () => call<UpdateInfo>("check_update"),
  openUrl: (url: string) => call<void>("open_url", { url }),
};

/* ── 更新检查 ───────────────────────────────────────────── */

export type UpdateInfo = {
  /** 当前平台对应的安装包；没有就是这次 release 没给本平台出包 */
  asset?: { name: string; url: string } | null;
  current: string;
  latest: string;
  tag?: string;
  has_update: boolean;
  url: string;
  name?: string;
  published_at?: string;
  notes?: string;
};

/* ── 线路（数据源） ─────────────────────────────────────── */

export type LineItem = { name?: string; url: string; host?: string };
export type LineState = { base: string; default: string; fixed: LineItem[] };
export type LineMirrors = { source: string; script?: string; items: LineItem[]; recommended_source?: string; recommended: LineItem[] };
export type LineProbe = { url: string; ok: boolean; status?: number; ms: number; error?: string };
export type LineHealth = { base: string; ok: boolean; ms: number; status: number | null };

/** 自动选线：只比 `.com` 与 `.ai` 两条，取通的那条、快的那条。 */
export type LineAutoCandidate = {
  tier: "main" | "cn" | "mirror";
  url: string;
  ok: boolean;
  reachable: boolean;
  ms: number;
  status: number | null;
};
export type LineAuto = {
  previous: string;
  base: string;
  switched: boolean;
  pick: {
    base: string;
    tier: "main" | "cn" | "none";
    ms: number;
    reason: string;
    candidates: LineAutoCandidate[];
  };
};

/** 回切主站的探测结果：`probe.ok` 为主站到底通不通 */
export type LineTryMain = {
  previous: string;
  base: string;
  switched: boolean;
  probe: { ok: boolean; base: string; tier: string; ms: number; status: number | null };
};

/** `https://a.b/c` → `a.b`（线路展示用） */
export const hostOf = (url: string) => {
  try { return new URL(url).host; } catch { return url.replace(/^https?:\/\//, "").replace(/\/.*$/, ""); }
};

/* ── 自检：验证 webview → 本地协议 → 上游 的取流链路 ───────── */
export async function selfTest(): Promise<string> {
  const steps: string[] = [];
  try {
    const info = await api.play(117, 3);
    steps.push(`play:${info?.video_url ? "ok" : "empty"}`);
    const hlsUrl = `${LOCAL}/hls?u=${encodeURIComponent(info.video_url)}`;
    const r1 = await fetch(hlsUrl);
    const txt = await r1.text();
    steps.push(`hls:${r1.status}/${txt.length}/${txt.startsWith("#EXTM3U") ? "m3u8" : "bad"}`);
    const keyLine = txt.split("\n").find((l) => l.startsWith("#EXT-X-KEY"));
    if (keyLine) {
      const m = keyLine.match(/URI="([^"]+)"/);
      if (m) {
        const rustUrl = m[1];
        steps.push(`rustUrl:${rustUrl.slice(0, 60)}`);
        // A/B：用 JS 重新编码同一条上游地址
        const inner = decodeURIComponent(rustUrl.split("?u=")[1] || "");
        steps.push(`inner:${inner.slice(0, 50)}`);
        try {
          const rk = await fetch(rustUrl);
          const bk = await rk.arrayBuffer();
          steps.push(`keyRust:${rk.status}/${bk.byteLength}`);
        } catch (e) { steps.push(`keyRust:throw:${(e as Error).message.slice(0, 30)}`); }
        try {
          const rk2 = await fetch(`${LOCAL}/seg?u=${encodeURIComponent(inner)}`);
          const bk2 = await rk2.arrayBuffer();
          steps.push(`keyJs:${rk2.status}/${bk2.byteLength}`);
        } catch (e) { steps.push(`keyJs:throw:${(e as Error).message.slice(0, 30)}`); }
      }
    }
    const segLine = txt.split("\n").find((l) => l && !l.startsWith("#") && l.includes("/seg?u="));
    if (segLine) {
      try {
        const r2 = await fetch(segLine.trim());
        const buf = await r2.arrayBuffer();
        steps.push(`seg:${r2.status}/${buf.byteLength}`);
      } catch (e) { steps.push(`seg:throw:${(e as Error).message.slice(0, 40)}`); }
    } else steps.push("seg:no-line");
  } catch (e) {
    steps.push(`err:${(e as Error).message.slice(0, 60)}`);
  }
  return steps.join(" | ");
}

/* ── 格式化 ─────────────────────────────────────────────── */

export const fmt = (n: number | undefined) => {
  const v = Number(n) || 0;
  if (v >= 100_000_000) return `${(v / 100_000_000).toFixed(1).replace(/\.0$/, "")}亿`;
  if (v >= 10_000) return `${(v / 10_000).toFixed(1).replace(/\.0$/, "")}万`;
  return String(v);
};

export const fmtTime = (s: number) => {
  if (!isFinite(s) || s < 0) s = 0;
  const m = Math.floor(s / 60);
  const ss = Math.floor(s % 60);
  return `${m}:${ss.toString().padStart(2, "0")}`;
};

export const dur = (s?: number | string) => {
  const n = Number(s) || 0;
  if (n <= 0) return "";
  const m = Math.floor(n / 60);
  const ss = n % 60;
  return `${m}:${String(ss).padStart(2, "0")}`;
};

/** 非 m3u8 媒体（mp4 等）走 /seg 原样透传 */
export function directMedia(url: string): string {
  return `${LOCAL}/seg?u=${encodeURIComponent(url)}`;
}

/** 按扩展名选择代理方式：m3u8 → 播放列表改写；其它 → 原样透传 */
export function mediaUrl(url: string): string {
  return /\.m3u8(\?|$)/i.test(url) ? playlist(url) : directMedia(url);
}

<script lang="ts">
  import { tick } from "svelte";
  import { invoke } from "@tauri-apps/api/core";

  let { path }: { path: string } = $props();

  let audio: HTMLAudioElement | undefined = $state();
  let src = $state<string | null>(null);
  let busy = $state(false);
  let failed = $state(false);
  let playing = $state(false);
  let current = $state(0);
  let duration = $state(0);

  /** Type the media element expects, from the stored file extension. */
  function mime(path: string) {
    const extension = path.slice(path.lastIndexOf(".") + 1).toLowerCase();
    if (extension === "mp3") return "audio/mpeg";
    if (extension === "m4a" || extension === "aac" || extension === "mp4") return "audio/mp4";
    if (extension === "wav") return "audio/wav";
    return "audio/ogg";
  }

  /**
   * Play/pause, loading the file on first use.
   *
   * The bytes come through IPC because WebKitGTK's media pipeline cannot load
   * the custom asset scheme that `convertFileSrc` produces. That also means the
   * file is only read when the user actually asks to hear it.
   */
  async function toggle() {
    if (busy) return;
    if (!src) {
      busy = true;
      failed = false;
      try {
        const data = await invoke<string>("read_file", { path });
        const bytes = Uint8Array.from(atob(data), (c) => c.charCodeAt(0));
        src = URL.createObjectURL(new Blob([bytes], { type: mime(path) }));
        // The element only has a source once this has rendered.
        await tick();
      } catch {
        failed = true;
        busy = false;
        return;
      }
      busy = false;
    }
    if (!audio) {
      failed = true;
      return;
    }
    if (audio.paused) await audio.play().catch(() => (failed = true));
    else audio.pause();
  }

  function seek(event: Event) {
    if (!audio) return;
    audio.currentTime = Number((event.currentTarget as HTMLInputElement).value);
  }

  function clock(seconds: number) {
    if (!Number.isFinite(seconds)) return "0:00";
    const minutes = Math.floor(seconds / 60);
    const rest = Math.floor(seconds % 60);
    return `${minutes}:${String(rest).padStart(2, "0")}`;
  }
</script>

<div class="player">
  <button
    class="toggle"
    class:failed
    onclick={toggle}
    title={failed ? "Could not play this file" : playing ? "Pause" : "Play"}
    disabled={busy}>
    {busy ? "…" : failed ? "⚠" : playing ? "⏸" : "▶"}
  </button>
  <input
    class="progress"
    type="range"
    min="0"
    max={duration || 0}
    step="0.1"
    value={current}
    oninput={seek}
    aria-label="Seek"
  />
  <span class="elapsed">{clock(current)}</span>
  <audio
    bind:this={audio}
    src={src ?? undefined}
    preload="metadata"
    onloadedmetadata={() => (duration = audio?.duration ?? 0)}
    ontimeupdate={() => (current = audio?.currentTime ?? 0)}
    onplay={() => (playing = true)}
    onpause={() => (playing = false)}
    onerror={() => (failed = true)}
    onended={() => {
      playing = false;
      current = 0;
    }}
  ></audio>
</div>

<style>
  .player {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 240px;
    max-width: 100%;
  }
  .toggle {
    flex: none;
    width: 28px;
    height: 28px;
    border: 0;
    border-radius: 999px;
    background: #14532d;
    color: #e4e4e7;
    cursor: pointer;
    font-size: 12px;
  }
  .toggle.failed {
    background: #7f1d1d;
  }
  .toggle:disabled {
    cursor: default;
  }
  .progress {
    flex: 1;
    min-width: 0;
    accent-color: #22c55e;
  }
  .elapsed {
    flex: none;
    font-size: 11px;
    color: #a1a1aa;
    font-variant-numeric: tabular-nums;
  }
</style>

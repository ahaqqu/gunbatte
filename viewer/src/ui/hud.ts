/** HTML HUD: top stats, kill feed, legend, winner modal. */

import { botColor, BOT_COLORS, fmtTime } from "../types.js";

/** Bot names are operator-controlled wire data (the server allows spaces and
 * dots, and older replays predate any charset rule): escape before they touch
 * innerHTML so a crafted name cannot carry HTML into a spectator's browser. */
function esc(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

export class Hud {
  topbar = document.getElementById("topbar")!;
  matchSub = document.getElementById("match-sub")!;
  statAlive = document.getElementById("stat-alive")!;
  statZone = document.getElementById("stat-zone")!;
  statTime = document.getElementById("stat-time")!;
  killfeed = document.getElementById("killfeed")!;
  legend = document.getElementById("legend")!;
  winnerModal = document.getElementById("winner-modal")!;
  winnerName = document.getElementById("winner-name")!;
  winnerSub = document.getElementById("winner-sub")!;
  podium = document.getElementById("podium")!;
  private feedRows: HTMLElement[] = [];

  showAll(): void {
    for (const el of [this.topbar, this.killfeed, this.legend]) el.classList.remove("hidden");
  }

  /** Live play omits the seed (issue #69): it is the match's one secret and
   * can never be shown truthfully there — only replays carry it. */
  setHeader(names: string[], mapId: string, seed?: number): void {
    const seedPart = seed === undefined ? "" : ` · seed ${seed}`;
    this.matchSub.textContent = `map ${mapId}${seedPart} · ${names.length} entrants`;
  }

  stats(tick: number, alive: number, zonePhase: number, shrinking: boolean): void {
    this.statAlive.textContent = String(alive);
    this.statTime.textContent = fmtTime(tick / 10);
    this.statZone.textContent = shrinking ? `P${zonePhase} ⚠` : `P${zonePhase}`;
    this.statZone.style.color = shrinking ? "#e6455f" : "#e0457f";
  }

  kill(killer: number | null, victim: number, names: string[]): void {
    const row = document.createElement("div");
    row.className = "feed-row";
    if (killer === null) {
      row.innerHTML = `<span class="zone-kill">☠ zone</span> <span class="victim" style="color:${botColor(victim)}">${esc(names[victim])}</span>`;
    } else {
      row.innerHTML = `<span style="color:${botColor(killer)}">${esc(names[killer])}</span> ⚡ <span class="victim" style="color:${botColor(victim)}">${esc(names[victim])}</span>`;
    }
    this.killfeed.appendChild(row);
    this.feedRows.push(row);
    while (this.feedRows.length > 6) {
      const old = this.feedRows.shift()!;
      old.classList.add("fading");
      setTimeout(() => old.remove(), 700);
    }
    setTimeout(() => { row.classList.add("fading"); setTimeout(() => row.remove(), 700); }, 7000);
  }

  buildLegend(names: string[], onSelect: (bot: number) => void): void {
    this.legend.innerHTML = "";
    names.forEach((name, bot) => {
      const row = document.createElement("div");
      row.className = "legend-row";
      row.innerHTML = `<span class="legend-dot" style="background:${botColor(bot)};color:${botColor(bot)}"></span><span>${esc(name)}</span><span class="legend-elim" data-bot="${bot}"></span>`;
      row.addEventListener("click", () => onSelect(bot));
      this.legend.appendChild(row);
    });
  }

  legendDead(bot: number): void {
    const row = this.legend.children[bot] as HTMLElement | undefined;
    if (row) row.classList.add("dead");
  }

  legendElim(bot: number, text: string): void {
    const row = this.legend.children[bot] as HTMLElement | undefined;
    if (row) row.querySelector(".legend-elim")!.textContent = text;
  }

  legendActive(bot: number | null): void {
    for (let i = 0; i < this.legend.children.length; i++) {
      (this.legend.children[i] as HTMLElement).classList.toggle("active", i === bot);
    }
  }

  winner(winner: number | null, names: string[], placements: number[]): void {
    if (winner === null) {
      this.winnerName.textContent = "NOBODY";
      this.winnerName.style.color = "#8d82b5";
    } else {
      this.winnerName.textContent = names[winner];
      this.winnerName.style.color = botColor(winner);
    }
    this.podium.innerHTML = "";
    placements.slice(0, 5).forEach((bot, rank) => {
      const li = document.createElement("li");
      li.innerHTML = `<span class="rank">#${rank + 1}</span><span style="color:${botColor(bot)}">${esc(names[bot])}</span>`;
      this.podium.appendChild(li);
    });
    this.winnerModal.classList.remove("hidden");
    this.confetti();
  }

  /** CSS confetti shower across the winner modal. */
  private confetti(): void {
    for (let i = 0; i < 90; i++) {
      const p = document.createElement("i");
      p.className = "confetti-piece";
      const s = 6 + Math.random() * 9;
      p.style.left = `${Math.random() * 100}%`;
      p.style.width = `${s}px`;
      p.style.height = `${s * 0.55}px`;
      p.style.background = BOT_COLORS[i % BOT_COLORS.length];
      p.style.animationDelay = `${Math.random() * 1.4}s`;
      p.style.animationDuration = `${2.4 + Math.random() * 2.2}s`;
      this.winnerModal.appendChild(p);
      setTimeout(() => p.remove(), 7000);
    }
  }

  hideWinner(): void {
    this.winnerModal.classList.add("hidden");
  }
}

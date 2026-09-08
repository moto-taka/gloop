import fs from 'node:fs';
import xterm from '@xterm/headless';

const lines = fs.readFileSync('public/terminal.cast', 'utf8').trim().split('\n').map(JSON.parse);
const [header, ...events] = lines;
const terminal = new xterm.Terminal({cols: header.width, rows: header.height, allowProposedApi: true});
const palette = ['#131b27', '#f67f87', '#9ce0bd', '#e8cd89', '#86b9ee', '#bda4eb', '#7edcdd', '#dbe3ed',
  '#748195', '#ff99a1', '#b4f1ce', '#ffe5a2', '#a1ceff', '#d1b9ff', '#a0f4f4', '#ffffff'];
function color(cell, fg) {
  if (fg ? cell.isFgDefault() : cell.isBgDefault()) return fg ? '#dbe3ed' : 'transparent';
  const n = fg ? cell.getFgColor() : cell.getBgColor();
  if (fg ? cell.isFgRGB() : cell.isBgRGB()) return '#' + n.toString(16).padStart(6, '0');
  if (n < 16) return palette[n];
  if (n >= 232) {const c = (8 + (n - 232) * 10).toString(16).padStart(2, '0'); return '#' + c.repeat(3);}
  const levels = [0, 95, 135, 175, 215, 255];
  return '#' + [Math.floor((n - 16) / 36), Math.floor((n - 16) / 6) % 6, (n - 16) % 6]
    .map(v => levels[v].toString(16).padStart(2, '0')).join('');
}
function snapshot() {
  return Array.from({length: header.height}, (_, row) => {
    const line = terminal.buffer.active.getLine(terminal.buffer.active.viewportY + row);
    const runs = [];
    for (let col = 0; col < header.width; col++) {
      const cell = line.getCell(col);
      if (!cell || cell.getWidth() === 0) continue;
      let fg = color(cell, true), bg = color(cell, false);
      if (cell.isInverse()) [fg, bg] = [bg === 'transparent' ? '#131b27' : bg, fg];
      const style = {fg, bg, bold: !!cell.isBold(), dim: !!cell.isDim()};
      const previous = runs.at(-1);
      if (previous && JSON.stringify(previous.style) === JSON.stringify(style)) previous.text += cell.getChars() || ' ';
      else runs.push({text: cell.getChars() || ' ', style});
    }
    return runs;
  });
}
let index = 0, previous = '', frames = [];
for (let tick = 0; tick <= header.duration * 10; tick++) {
  while (index < events.length && events[index][0] <= tick / 10) {
    await new Promise(resolve => terminal.write(events[index++][2], resolve));
  }
  const rows = snapshot();
  const value = JSON.stringify(rows);
  if (value !== previous) { frames.push({time: tick / 10, rows}); previous = value; }
  if ([5, 10, 15, 22, 27].includes(tick / 10)) {
    console.log(`FRAME ${tick / 10}s\n` + rows.map(row => row.map(run => run.text).join('')).join('\n'));
  }
}
fs.writeFileSync('public/terminal-frames.json', JSON.stringify({width: header.width, height: header.height, frames}));
console.log(`${frames.length} distinct terminal snapshots`);

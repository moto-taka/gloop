import fs from 'node:fs';
import {bundle} from '@remotion/bundler';
import {renderMedia, renderStill, selectComposition} from '@remotion/renderer';

const serveUrl = await bundle({entryPoint: 'src/index.tsx'});
fs.mkdirSync('out', {recursive: true});
for (const id of ['Terminal', 'Parallel']) {
  const composition = await selectComposition({serveUrl, id});
  if (process.argv.includes('--stills')) {
    for (const frame of id === 'Terminal' ? [15, 160, 320, 660, 825] : [95, 240, 365, 455, 555]) {
      await renderStill({serveUrl, composition, output: `out/${id}-${frame}.png`, frame});
    }
  } else {
    await renderMedia({serveUrl, composition, codec: 'h264', pixelFormat: 'yuv420p',
      outputLocation: `../assets/videos/gloop-${id.toLowerCase()}.mp4`, crf: 18,
      concurrency: 4, muted: true, onProgress: ({progress}) => {
        if (progress === 1) console.log(`${id}: render complete`);
      }});
    console.log(`${id}: MP4 saved`);
  }
}

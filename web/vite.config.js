import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import path from 'path';
import fs from 'fs';
import http from 'http';

const TILE_SERVER_PORT = 5176;

function serveTile(tilesDir, req, res) {
  const reqUrl = req.url;
  const filePath = path.join(tilesDir, decodeURIComponent(reqUrl.replace(/^\/+/, '')));
  if (!filePath.startsWith(tilesDir) || !fs.existsSync(filePath)) {
    res.writeHead(404);
    res.end('Not found');
    return;
  }

  const stat = fs.statSync(filePath);
  const corsHeaders = {
    'Access-Control-Allow-Origin':   '*',
    'Access-Control-Expose-Headers': 'Content-Length, Content-Range, Accept-Ranges, ETag',
  };

  const rangeHeader = req.headers['range'];
  if (rangeHeader) {
    const [, startStr, endStr] = rangeHeader.match(/bytes=(\d+)-(\d*)/) ?? [];
    const start  = parseInt(startStr, 10);
    const end    = endStr ? parseInt(endStr, 10) : stat.size - 1;
    const length = end - start + 1;
    console.log(`[tiles] range ${start}-${end} (${length} bytes) ${path.basename(filePath)}`);
    res.writeHead(206, {
      ...corsHeaders,
      'Accept-Ranges':  'bytes',
      'Content-Range':  `bytes ${start}-${end}/${stat.size}`,
      'Content-Length': length,
      'Content-Type':   'application/octet-stream',
    });
    // createReadStream with explicit start/end handles keep-alive framing correctly
    fs.createReadStream(filePath, { start, end }).pipe(res);
  } else {
    console.log(`[tiles] full file ${path.basename(filePath)} (${stat.size} bytes)`);
    res.writeHead(200, {
      ...corsHeaders,
      'Accept-Ranges':  'bytes',
      'Content-Length': stat.size,
      'Content-Type':   'application/octet-stream',
    });
    fs.createReadStream(filePath).pipe(res);
  }
}

export default defineConfig({
  plugins: [react(), {
    name: 'tile-server',
    configureServer() {
      const tilesDir = path.resolve(__dirname, '../out');

      const srv = http.createServer((req, res) => {
        if (req.method === 'OPTIONS') {
          res.writeHead(204, {
            'Access-Control-Allow-Origin':   '*',
            'Access-Control-Allow-Headers':  'Range, If-None-Match',
            'Access-Control-Allow-Methods':  'GET, HEAD, OPTIONS',
            'Access-Control-Max-Age':        '86400',
          });
          res.end();
          return;
        }
        serveTile(tilesDir, req, res);
      });

      srv.listen(TILE_SERVER_PORT, '0.0.0.0', () => {
        console.log(`  ➜  Tiles:   http://localhost:${TILE_SERVER_PORT}/`);
      });
    },
  }],
});

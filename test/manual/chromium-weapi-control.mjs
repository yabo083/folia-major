import { chromium } from 'playwright';

const browser = await chromium.launch({ proxy: { server: 'http://127.0.0.1:7890' }, headless: true });
const page = await browser.newPage({ userAgent: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36 Edg/124.0.0.0' });

page.on('response', async (r) => {
  const u = r.url();
  if (/weapi.*(song|playlist\/detail)|api\/v3\/song|api\/song/.test(u)) {
    let t = '';
    try { t = await r.text(); } catch (e) { t = 'TEXT_ERR'; }
    const h = await r.allHeaders().catch(() => ({}));
    console.log(`RESP ${u} status=${r.status()} len=${t.length} ct=${h['content-type']} enc=${h['content-encoding']} body=${t.slice(0, 120)}`);
  }
});
page.on('request', async (r) => {
  const u = r.url();
  if (/weapi.*(song|playlist\/detail)|api\/v3\/song|api\/song/.test(u)) {
    const h = await r.allHeaders().catch(() => ({}));
    console.log(`REQ ${r.method()} ${u}`);
    console.log(`  referer=${h['referer']} cookie=${(h['cookie']||'').split(';').map(s=>s.trim().split('=')[0]).join(',')}`);
    if (r.postData()) console.log(`  POST=${r.postData().slice(0, 100)}`);
  }
});

await page.goto('https://music.163.com/#/song?id=33894312', { waitUntil: 'networkidle', timeout: 60000 });
await page.waitForTimeout(5000);
await browser.close();

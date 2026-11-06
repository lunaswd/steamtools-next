// @vitest-environment happy-dom
import { beforeEach, expect, it } from 'vitest';
// @ts-expect-error Node fs is available in Vitest; UI-only TS config has no Node ambient types.
import { readFileSync } from 'node:fs';
// Vitest deliberately stubs CSS imports; read the real sheets instead.
const base=readFileSync('src/style.css','utf8');
const density=readFileSync('src/content-density.css','utf8');

beforeEach(()=>{
  (window as unknown as {happyDOM:{setViewport:(viewport:{width:number;height:number})=>void}}).happyDOM.setViewport({width:1360,height:880});
  document.documentElement.removeAttribute('data-density');
  document.head.innerHTML=`<style>${base}\n${density}</style>`;
  document.body.innerHTML='';
});
const fixture=(page:string)=>`<div class="app-shell"><aside class="sidebar"><nav><button class="nav-item active" aria-label="${page}"><span>${page}</span></button></nav></aside><div class="workspace"><header class="topbar"><div class="topbar-title"><h1>${page}</h1></div><div class="topbar-actions"><button class="button">Refresh</button></div></header><main data-page="${page}"><div class="page-content"><section class="surface">Main content</section></div><footer class="page-footer">Scan status</footer></main></div></div>`;
const style=(selector:string)=>getComputedStyle(document.querySelector(selector)!);

it('reserves at most 90px for desktop page chrome across every page',()=>{
  for(const page of ['kernel','library','import','manifest','extract','achievements','overview','diagnostics','settings']){
    document.body.innerHTML=fixture(page);
    // A computed-style contract, not a browser screenshot/geometry claim.
    const chrome=parseFloat(style('.topbar').height)+parseFloat(style('main').paddingTop);
    expect(chrome).toBeLessThanOrEqual(90);
    expect(style('.surface').paddingTop).toBe('14px');
    expect(style('.topbar-title h1').whiteSpace).toBe('nowrap');
  }
});
it('keeps errors and operation status visible while compact mode further reduces chrome',()=>{
  document.body.innerHTML=fixture('achievements');
  document.querySelector('main')!.insertAdjacentHTML('afterbegin','<div class="error-banner" role="alert">Error details</div><div class="progress-strip" role="status">Working</div><div class="session-only-banner">Settings not saved</div>');
  for(const selector of ['.error-banner','.progress-strip','.session-only-banner'])expect(style(selector).display).not.toBe('none');
  const normal=parseFloat(style('.topbar').height)+parseFloat(style('main').paddingTop);
  document.documentElement.dataset.density='compact';
  const compact=parseFloat(style('.topbar').height)+parseFloat(style('main').paddingTop);
  expect(compact).toBeLessThan(normal);
});

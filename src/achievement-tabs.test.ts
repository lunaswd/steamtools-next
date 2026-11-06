import { describe, expect, it } from 'vitest';
import { AchievementTabs } from './achievement-tabs';

describe('achievement browser tab lifecycle',()=>{
  it('deduplicates games and preserves each view independently',()=>{
    const tabs=new AchievementTabs<string>();const first=tabs.open(42);
    first.selection.add('FIRST');first.query='hidden';first.filter='locked';first.scrollTop=750;
    const second=tabs.open(43);second.selection.add('SECOND');second.query='other';
    expect(tabs.open(42)).toBe(first);expect(tabs.all).toHaveLength(2);
    expect(tabs.active?.scrollTop).toBe(750);expect(tabs.active?.selection.has('SECOND')).toBe(false);
    expect(tabs.select(null)).toBe(true);expect(tabs.active).toBeNull();
    expect(tabs.select(43)).toBe(true);expect(tabs.active?.query).toBe('other');
  });
  it('closes to an adjacent tab, then the fixed game library',()=>{
    const tabs=new AchievementTabs<string>();tabs.open(42);tabs.open(43);tabs.open(44);
    tabs.select(43);expect(tabs.close(43)).toBe(true);expect(tabs.activeId).toBe(44);
    tabs.close(42);expect(tabs.activeId).toBe(44);tabs.close(44);expect(tabs.activeId).toBeNull();
  });
  it('ignores old responses after closing and reopening the same AppID',()=>{
    const tabs=new AchievementTabs<string>();const original=tabs.open(42);original.loading=true;
    tabs.close(42);const reopened=tabs.open(42);
    expect(tabs.contains(original)).toBe(false);expect(tabs.contains(reopened)).toBe(true);
    expect(reopened.selection.size).toBe(0);expect(reopened.catalog).toBeNull();
  });
  it('protects a writing tab from close without blocking other tabs',()=>{
    const tabs=new AchievementTabs<string>();const writing=tabs.open(42);writing.applying=true;
    tabs.open(43);expect(tabs.close(42)).toBe(false);expect(tabs.close(43)).toBe(true);
    expect(tabs.activeId).toBe(42);writing.applying=false;expect(tabs.close(42)).toBe(true);
  });
  it('invalidates pending reads for locale/root changes and bounds scroll values',()=>{
    const tabs=new AchievementTabs<string>();const tab=tabs.open(42);tab.loading=true;tab.request=3;tab.selection.add('FIRST');
    tabs.setScroll('42',Infinity);expect(tab.scrollTop).toBe(0);tabs.setScroll('library',120);expect(tabs.libraryScrollTop).toBe(120);
    tabs.invalidate(true);expect(tab.request).toBe(4);expect(tab.loading).toBe(false);expect(tab.selection.size).toBe(0);
  });
});

import type { AchievementCatalog } from './types';

export interface AchievementTab<Failure> {
  appId: number;
  catalog: AchievementCatalog|null;
  locale: string;
  steamPath: string|null;
  loading: boolean;
  applying: boolean;
  error: Failure|null;
  warning: Failure|null;
  selection: Set<string>;
  filter: string;
  query: string;
  scrollTop: number;
  request: number;
}

/** UI tabs, not Steam operations. Closing discards view state only. Captured
 * object identities distinguish a closed/reopened tab from its old request. */
export class AchievementTabs<Failure> {
  private tabs = new Map<number,AchievementTab<Failure>>();
  activeId: number|null = null;
  libraryScrollTop = 0;
  get active(){return this.activeId===null?null:this.tabs.get(this.activeId)||null}
  get all(){return [...this.tabs.values()]}
  get(appId:number){return this.tabs.get(appId)||null}
  contains(tab:AchievementTab<Failure>){return this.tabs.get(tab.appId)===tab}
  open(appId:number){
    if(!Number.isInteger(appId)||appId<=0)throw new Error('Invalid game tab AppID');
    let tab=this.tabs.get(appId);
    if(!tab){
      tab={appId,catalog:null,locale:'',steamPath:null,loading:false,applying:false,error:null,warning:null,selection:new Set(),filter:'all',query:'',scrollTop:0,request:0};
      this.tabs.set(appId,tab);
    }
    this.activeId=appId;
    return tab;
  }
  select(appId:number|null){
    if(appId!==null&&!this.tabs.has(appId))return false;
    this.activeId=appId;return true;
  }
  close(appId:number){
    const tab=this.tabs.get(appId);
    if(!tab||tab.applying)return false;
    const ids=[...this.tabs.keys()],position=ids.indexOf(appId);
    ++tab.request;this.tabs.delete(appId);
    if(this.activeId===appId)this.activeId=ids[position+1]??ids[position-1]??null;
    return true;
  }
  setScroll(id:string,top:number){
    const value=Math.max(0,Number.isFinite(top)?top:0);
    if(id==='library')this.libraryScrollTop=value;
    else {const tab=this.tabs.get(Number(id));if(tab)tab.scrollTop=value}
  }
  invalidate(clearSelection=false){
    for(const tab of this.tabs.values()){
      ++tab.request;tab.loading=false;tab.catalog=null;tab.locale='';tab.error=null;tab.warning=null;
      if(clearSelection)tab.selection.clear();
    }
  }
}

import { createElement, Monitor, Settings2, ArrowUpRight, Search, ShieldCheck, Copy, RefreshCw, Smartphone, Activity, ChevronRight, X, Wifi, CircleHelp, LogOut, Link, Gamepad2 } from 'lucide';
const icons={monitor:Monitor,settings:Settings2,arrow:ArrowUpRight,search:Search,shield:ShieldCheck,copy:Copy,refresh:RefreshCw,device:Smartphone,activity:Activity,chevron:ChevronRight,close:X,wifi:Wifi,help:CircleHelp,logout:LogOut,link:Link,gamepad:Gamepad2};
export function icon(name:keyof typeof icons):string {
  return createElement(icons[name],{width:20,height:20,'stroke-width':1.7,'aria-hidden':'true',focusable:'false'}).outerHTML;
}

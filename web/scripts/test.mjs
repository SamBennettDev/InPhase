import { readdirSync } from 'node:fs';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
// Explicit discovery works on Node 22/24 and Windows without shell globbing.
function discover(dir) {
  return readdirSync(dir,{withFileTypes:true}).flatMap(e => {
    const p=join(dir,e.name); return e.isDirectory()?discover(p):e.name.endsWith('.test.ts')?[p]:[];
  });
}
const files=discover('src').sort();
if(!files.length) throw new Error('No TypeScript tests found');
const result=spawnSync(process.execPath,['--import','tsx','--test',...files],{stdio:'inherit'});
if(result.error) throw result.error;
process.exit(result.status??1);

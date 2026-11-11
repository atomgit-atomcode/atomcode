const {spawn}=require('node:child_process');
const fs=require('node:fs');
const readline=require('node:readline');
const options=JSON.parse(process.env.ATOMCODE_BROWSER_FAULT_FIXTURE);
fs.writeFileSync(options.pid,String(process.pid));
fs.appendFileSync(options.spawns,String(process.pid)+'\n');
const child=spawn(process.execPath,[options.server,...process.argv.slice(2)],{stdio:['pipe','pipe','ignore']});
const clicks=new Set();
let dropping=false, deadline;
function stop() {
  child.kill('SIGTERM');
  deadline ||= setTimeout(()=>process.exit(1),6000);
}
const input=readline.createInterface({input:process.stdin});
input.on('line',line=>{
  const m=JSON.parse(line);
  if(m.method==='tools/call' && m.params.name==='browser_click') {
    clicks.add(m.id);fs.appendFileSync(options.calls,'click\n');
  }
  if(!child.stdin.destroyed) child.stdin.write(line+'\n');
});
input.on('close',()=>child.stdin.end());
child.stdin.on('error',()=>{});
readline.createInterface({input:child.stdout}).on('line',line=>{
  const m=JSON.parse(line);
  if(clicks.has(m.id) && m.result) {
    fs.writeFileSync(options.late,JSON.stringify({is_error:!!m.result.isError}));
    if(options.drop && !m.result.isError && !fs.existsSync(options.marker)) {
      fs.writeFileSync(options.marker,'one successful reply withheld');dropping=true;stop();return;
    }
  }
  if(!dropping) process.stdout.write(line+'\n');
});
child.on('exit',code=>{clearTimeout(deadline);process.exit(code||0);});
process.on('SIGTERM',stop);
process.on('SIGINT',stop);

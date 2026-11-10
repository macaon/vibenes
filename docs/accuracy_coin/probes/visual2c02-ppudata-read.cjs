// Run against the public Visual 2C02 sources (not vendored here).
// Usage: node visual2c02-ppudata-read.cjs <source-directory> [phase-offset-half-ticks=0]
// See ../ppudata-refill.md for source URLs and scope.
const fs = require('fs');
const vm = require('vm');
const crypto = require('crypto');
const base = process.argv[2];
if (!base) throw Error('Pass the directory containing the Visual 2C02 sources');
const c = {console, alert: m => { throw Error(m); }, window: {}, setTimeout, clearTimeout};
vm.createContext(c);
for (const f of ['nodenames.js', 'segdefs.js', 'transdefs.js', 'wires.js', 'chipsim.js', 'macros.js', 'states.js', 'sprtable.js']) {
  const source = fs.readFileSync(base + '/' + f);
  console.log('sha256', f, crypto.createHash('sha256').update(source).digest('hex'));
  vm.runInContext(source.toString('utf8'), c, {filename: f});
}
c.refresh = () => {};
c.arrayContains = (a, x) => a.indexOf(x) !== -1;
c.setupNodes();
c.setupTransistors();
c.setState(c.resetStatePrerenderEven);
console.log('initial',c.readBits('hpos',9),c.readBits('vpos',9),c.readBit('bkg_enable'),c.readBit('spr_enable'),c.readBit('pclk0'));
c.mRead = () => 0;
c.mWrite = () => {};
let half = 0;
function tick() {
  c.writeBit('clk0', !c.readBit('clk0'));
  c.handleChrBus();
  half++;
}

c.mRead = addr => (addr & 255) ^ 0xA5;
function writeRegister(addr,value) {
 c.writeBits('io_ab',3,addr); c.writeBits('io_db',8,value); c.setLow('io_rw'); c.setLow('io_ce');
 for(let i=0;i<15;i++) tick();
 c.setHigh('io_ce'); c.setHigh('io_rw'); c.floatBits('io_db',8);
 for(let i=0;i<9;i++) tick();
}
writeRegister(6,0x2C); writeRegister(6,0);
for(let i=0;i<32;i++) tick();
for(let i=0;i<Number(process.argv[3] || 0);i++) tick();
console.log('before read',c.readBits('vramaddr_v',15),c.readBits('inbuf',8));
c.writeBits('io_ab',3,7); c.floatBits('io_db',8); c.setHigh('io_rw'); c.setLow('io_ce');
for(let i=0;i<15;i++) tick();
c.setHigh('io_ce');
console.log('relative-half pclk0 v buffer ALE RD address');
let last='';
let refillHalf = null;
let incrementHalf = null;
for(let i=0;i<48;i++) {
 tick();
 let row=[c.readBit('pclk0'),c.readBits('vramaddr_v',15),c.readBits('inbuf',8),c.readBit('ale'),c.readBit('rd'),c.readAddressBus()].join(' ');
 if(row!==last) console.log(i+1,row);
 if(refillHalf===null && c.readBits('inbuf',8)===0xA5) refillHalf=i+1;
 if(incrementHalf===null && c.readBits('vramaddr_v',15)===0x2C01) incrementHalf=i+1;
 last=row;
}

console.log('refill half-tick:',refillHalf,'increment half-tick:',incrementHalf);
if(refillHalf===null || incrementHalf===null || refillHalf<17 || incrementHalf<refillHalf) throw Error('Invalid delayed refill sequence');

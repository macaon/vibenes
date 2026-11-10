// Run against the public Visual 2C02 sources (not vendored here).
// Usage: node visual2c02-oam-wrap.cjs <source-directory>
// See ../oam-primary-wrap.md for source URLs and scope.
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
for(let a=0;a<288;a++) c.sprite_write(a, a<8 ? 0 : (a===252 ? 0xAA : 0xFF), false);
c.writeBits('io_ab',3,1);
c.writeBits('io_db',8,8);
c.setLow('io_rw');
c.setLow('io_ce');
for(let i=0;i<24;i++) tick();
c.setHigh('io_ce'); c.setHigh('io_rw'); c.floatBits('io_db',8);
console.log('mask',c.readBit('bkg_enable'));

while (!(c.readBits('vpos',9)===0 && c.readBits('hpos',9)===190 && c.readBit('pclk0'))) tick();
c.writeBits('io_ab',3,4); c.floatBits('io_db',8); c.setHigh('io_rw'); c.setLow('io_ce');
console.log('half dot pclk0 io_data oam_address secondary_address');
let last='';
const observed = new Map();
for(let i=0;i<8*30;i++) {
 tick();
 let row=[c.readBits('hpos',9),c.readBit('pclk0'),c.readBits('io_db',8),c.readBits('spr_addr',8),c.readBits('spr_ptr',5)].join(' ');
 if(row!==last) console.log(half,row);
 if (!c.readBit('pclk0')) observed.set(c.readBits('hpos',9), [c.readBits('io_db',8),c.readBits('spr_ptr',5)]);
 last=row;
}

for(const [dot,expected] of [[206,0],[207,0xAA],[208,0],[209,0xAA],[210,0xFF],[211,0xAA]]) {
 const sample=observed.get(dot);
 if(!sample || sample[0]!==expected || sample[1]!==8) throw Error(`Unexpected sample at hpos ${dot}: ${sample}`);
}
console.log('PASS: primary/secondary alternation with only eight secondary bytes occupied');

// Run against the public Visual 2C02 sources (not vendored here).
// Usage: node visual2c02-overflow.cjs <source-directory> [sprite-count=9] [gap=0]
// See ../sprite-overflow-output-latch.md for source URLs and scope.
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
const count = Number(process.argv[3] || 9);
const gap = Number(process.argv[4] || 0);
for (let a=0;a<256;a++) {
 const sprite = a >> 2;
 const active = sprite < Math.min(count,8) || (count > 8 && sprite === 8+gap);
 c.sprite_write(a, active ? 0 : 255, false);
}
c.writeBits('io_ab',3,1);
c.writeBits('io_db',8,8);
c.setLow('io_rw');
c.setLow('io_ce');
for(let i=0;i<24;i++) tick();
c.setHigh('io_ce'); c.setHigh('io_rw'); c.floatBits('io_db',8);
console.log('mask',c.readBit('bkg_enable'));
let last = '';
let firstRise = null;
console.log('half hpos vpos pclk0 pclk1 overflow overflow_set oam_high oam_low oam_data');
for(let i=0;i<8*490;i++) {
 tick();
 const h=c.readBits('hpos',9), v=c.readBits('vpos',9);
 if (v===0 && c.readBit('spr_overflow') && firstRise===null) firstRise=h;
 if(v===0 && h>=126 && h<=134) {
  let row=[h,v,c.readBit('pclk0'),c.readBit('pclk1'),c.readBit('spr_overflow'),c.readBit('sec_oam_overflow'),c.readBits('spr_ptr',6),c.readBits('spr_addr',2),c.readBits('spr_d',8)].join(' ');
  if(row!==last) console.log(half,row);
  last=row;
 }
}
console.log('first overflow rise:', firstRise);
const expected = count === 8 ? null : 131 + 2*gap;
if (firstRise !== expected) throw Error(`Expected ${expected}, got ${firstRise}`);

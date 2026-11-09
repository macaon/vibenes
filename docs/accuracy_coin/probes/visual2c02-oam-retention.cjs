// Run against the public Visual 2C02 sources (not vendored here).
// Usage: node visual2c02-oam-retention.cjs <source-directory>
// See ../oam-row-handoff.md for source URLs and scope.
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
for (let a=0;a<288;a++) c.sprite_write(a, a===0 ? 0 : 255, false);
c.writeBits('io_ab',3,1);
c.writeBits('io_db',8,8);
c.setLow('io_rw');
c.setLow('io_ce');
for(let i=0;i<24;i++) tick();
c.setHigh('io_ce'); c.setHigh('io_rw'); c.floatBits('io_db',8);
console.log('mask',c.readBit('bkg_enable'));

while (!(c.readBits('vpos',9)===0 && c.readBits('hpos',9)===7 && c.readBit('pclk0'))) tick();
console.log('disable starts',half);
// RP2A03G M2 stays high for 15 master half-cycles, or 1.875 dots.
c.writeBits('io_ab',3,1); c.writeBits('io_db',8,0); c.setLow('io_rw'); c.setLow('io_ce');
console.log('half dot pclk0 disabled render4 ptr');
let last='';
for(let i=0;i<160;i++) {
 tick();
 if(i===14) {c.setHigh('io_ce'); c.setHigh('io_rw'); c.floatBits('io_db',8);}
 let row=[c.readBits('hpos',9),c.readBit('pclk0'),c.readBit('rendering_disabled'),c.readBit('rendering_4'),c.readBits('spr_ptr',5)].join(' ');
 if(row!==last) console.log(half,row);
 last=row;
}

console.log('retained OAM2 row:', c.readBits('spr_ptr',5));
if (c.readBits('spr_ptr',5)!==4) throw Error('Expected retained row 4');

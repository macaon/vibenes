// Run against the public Visual 2C02 sources (not vendored here).
// Usage: node visual2c02-oamdata-write.cjs <source-directory> [oam-address=1] [phase=idle|clear]
// See ../oamdata-rendering-access.md for source URLs and scope.
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
for(let a=0;a<288;a++) c.sprite_write(a,255,false);
c.writeBits('io_ab',3,1);
c.writeBits('io_db',8,8);
c.setLow('io_rw');
c.setLow('io_ce');
for(let i=0;i<24;i++) tick();
c.setHigh('io_ce'); c.setHigh('io_rw'); c.floatBits('io_db',8);
console.log('mask',c.readBit('bkg_enable'));

const initialAddress = Number(process.argv[3] || 1);
const clear = process.argv[4] === 'clear';
function writeRegister(addr, value) {
 c.writeBits('io_ab',3,addr); c.writeBits('io_db',8,value); c.setLow('io_rw'); c.setLow('io_ce');
 for(let i=0;i<15;i++) tick();
 c.setHigh('io_ce'); c.setHigh('io_rw'); c.floatBits('io_db',8);
 for(let i=0;i<9;i++) tick();
}
while (!(c.readBits('vpos',9)===(clear ? 0 : 261) && c.readBits('hpos',9)===(clear ? 20 : 321) && c.readBit('pclk0'))) tick();
console.log('before address write', c.readBits('hpos',9), c.readBits('spr_addr',8));
writeRegister(3,initialAddress);
if (c.readBits('spr_addr',8)!==initialAddress) throw Error('OAMADDR setup failed');
console.log('before OAMDATA write', c.readBits('hpos',9), c.readBits('spr_addr',8));
writeRegister(4,0);
for(let i=0;i<48;i++) tick();
console.log('after OAMDATA write', c.readBits('hpos',9), c.readBits('spr_addr',8));

const expected = (initialAddress + 4) & 0xFC;
if (c.readBits('spr_addr',8)!==expected) throw Error(`Expected OAMADDR ${expected}`);

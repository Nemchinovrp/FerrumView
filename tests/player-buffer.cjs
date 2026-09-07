const { readFileSync } = require('node:fs');
const { Script, runInNewContext } = require('node:vm');
const assert = require('node:assert/strict');
const { test } = require('node:test');
const { join } = require('node:path');

const html = readFileSync(join(__dirname, '../static/index.html'), 'utf8');
const script = html.match(/<script>([\s\S]*)<\/script>/)[1];
const startup = script.slice(script.indexOf('let playbackRequested = false;'),
  script.indexOf('instance.on(Hls.Events.ERROR'));

function player(ranges, currentTime = 0, revision = 1) {
  const handlers = {};
  let plays = 0;
  runInNewContext(startup, {
    current: 1, revision,
    video: { currentTime, buffered: {
      length: ranges.length, start: i => ranges[i][0], end: i => ranges[i][1],
    } },
    instance: { on: (event, handler) => { handlers[event] = handler; } },
    Hls: { Events: { BUFFER_APPENDED: 'append', BUFFER_EOS: 'end' } },
    play: () => { plays++; }, message: () => {},
  });
  return { handlers, plays: () => plays };
}

test('page JavaScript parses', () => { new Script(script); });
test('startup needs six contiguous seconds ahead', () => {
  for (const [ranges, position] of [[[[0, 3]], 0], [[[0, 3], [10, 13]], 0], [[[0, 10]], 7]]) {
    const p = player(ranges, position);
    p.handlers.append();
    assert.equal(p.plays(), 0);
  }
  const p = player([[0, 10]], 4);
  p.handlers.append();
  p.handlers.append();
  assert.equal(p.plays(), 1);
});
test('short finite video starts at end of buffering', () => {
  const p = player([[0, 2]]);
  p.handlers.append();
  assert.equal(p.plays(), 0);
  p.handlers.end();
  assert.equal(p.plays(), 1);
});
test('events from a replaced player cannot start playback', () => {
  const p = player([[0, 10]], 0, 2);
  p.handlers.append();
  p.handlers.end();
  assert.equal(p.plays(), 0);
});

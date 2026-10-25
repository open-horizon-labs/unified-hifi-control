const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');

const context = { repo: { owner: 'example', repo: 'uhc' } };
function fixture(overrides = {}) {
  const calls = [];
  const pr = { merged: false, draft: false, mergeable: true, mergeable_state: 'clean', node_id: 'PR_fake', head: { sha: 'fake-reviewed-head', ref: 'chore/lms-beta-feed' }, base: { ref: 'v4' }, ...overrides };
  const github = {
    rest: { pulls: {
      get: async (args) => { calls.push(['get', args]); return { data: pr }; },
      merge: async (args) => { calls.push(['merge', args]); return { data: { merged: true } }; },
    } },
    graphql: async (...args) => { calls.push(['graphql', ...args]); },
  };
  return { calls, pr, github, args: { github, context, number: 123, branch: 'chore/lms-beta-feed', sleep: async () => {} } };
}
function run(args) { return require('../.github/scripts/lms-feed-auto-merge.cjs')(args); }

test('feed workflows use the bundled API client, not an external gh binary', () => {
  const workflow = fs.readFileSync('.github/workflows/build.yml', 'utf8');
  for (const name of ['update-repo-xml', 'update-beta-repo-xml']) {
    const job = workflow.split(`\n  ${name}:`)[1].split(/\n  [a-z][\w-]*:/)[0];
    assert.ok(!job.includes('run: gh pr merge'), `${name} requires missing GitHub CLI`);
    assert.ok(job.includes('uses: actions/github-script@v9'), name);
    assert.ok(job.includes("require('./.github/scripts/lms-feed-auto-merge.cjs')"), name);
    assert.ok(job.includes('LMS_FEED_PR:'), name);
    assert.ok(!job.includes('pullRequest.data.node_id'), name);
  }
  assert.ok(workflow.includes('node --test tests/lms_feed_automerge.test.cjs'));
});

test('ready feed PR is squash merged at the inspected SHA', async () => {
  const f = fixture(); await run(f.args);
  assert.deepEqual(f.calls.map(c => c[0]), ['get', 'merge']);
  assert.equal(f.calls[1][1].sha, 'fake-reviewed-head');
  assert.equal(f.calls[1][1].merge_method, 'squash');
});

test('protected PR enables auto-merge using the actual node_id and preserves checks', async () => {
  const f = fixture({ mergeable_state: 'blocked' }); await run(f.args);
  assert.deepEqual(f.calls.map(c => c[0]), ['get', 'graphql']);
  assert.equal(f.calls[1][2].pullRequestId, 'PR_fake');
});

test('already merged PR is an idempotent no-op', async () => {
  const f = fixture({ merged: true }); await run(f.args);
  assert.deepEqual(f.calls.map(c => c[0]), ['get']);
});

for (const [name, overrides] of [
  ['conflict', { mergeable: false, mergeable_state: 'dirty' }],
  ['draft', { draft: true }],
  ['wrong base', { base: { ref: 'master' } }],
  ['wrong branch', { head: { ref: 'feature/unrelated', sha: 'fake' } }],
]) {
  test(`${name} is refused without mutation`, async () => {
    const f = fixture(overrides); await assert.rejects(run(f.args));
    assert.deepEqual(f.calls.map(c => c[0]), ['get']);
  });
}

test('unknown mergeability is retried, then refused without mutation', async () => {
  const f = fixture({ mergeable: null, mergeable_state: 'unknown' });
  await assert.rejects(run(f.args));
  assert.equal(f.calls.filter(c => c[0] === 'get').length, 3);
  assert.ok(f.calls.every(c => c[0] === 'get'));
});

test('unknown mergeability can settle before merge', async () => {
  const f = fixture({ mergeable: null, mergeable_state: 'unknown' });
  f.args.sleep = async () => { f.pr.mergeable = true; f.pr.mergeable_state = 'clean'; };
  await run(f.args);
  assert.deepEqual(f.calls.map(c => c[0]), ['get', 'get', 'merge']);
});

test('unsuccessful merge response fails instead of reporting delivery', async () => {
  const f = fixture();
  f.github.rest.pulls.merge = async () => ({ data: { merged: false, message: 'Checks changed' } });
  await assert.rejects(run(f.args), /Checks changed/);
});

test('head changed after inspection is not merged and never falls back to bypass', async () => {
  const f = fixture();
  f.github.rest.pulls.merge = async () => { throw new Error('Head SHA changed'); };
  await assert.rejects(run(f.args), /Head SHA changed/);
  assert.ok(!f.calls.some(c => c[0] === 'graphql'));
});

test('stable feed uses the same API-only merge path', async () => {
  const f = fixture({ head: { ref: 'chore/lms-update', sha: 'fake-stable-head' } });
  f.args.branch = 'chore/lms-update'; await run(f.args);
  assert.equal(f.calls[1][0], 'merge');
  assert.equal(f.calls[1][1].sha, 'fake-stable-head');
});

test('auto-merge API refusal is propagated without bypassing checks', async () => {
  const f = fixture({ mergeable_state: 'blocked' });
  f.github.graphql = async () => { throw new Error('Auto-merge disabled'); };
  await assert.rejects(run(f.args), /Auto-merge disabled/);
  assert.ok(!f.calls.some(c => c[0] === 'merge'));
});

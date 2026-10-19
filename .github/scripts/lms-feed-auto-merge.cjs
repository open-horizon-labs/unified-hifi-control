// actions/github-script supplies the authenticated client; no host CLI is needed.
module.exports = async function mergeFeed({ github, context, number, branch,
  sleep = (ms) => new Promise(resolve => setTimeout(resolve, ms)) }) {
  if (!Number.isInteger(number) || number <= 0) throw new Error('Invalid feed PR number');
  const target = { ...context.repo, pull_number: number };
  let pr;
  for (let attempt = 0; attempt < 3; attempt++) {
    ({ data: pr } = await github.rest.pulls.get(target));
    if (pr.merged) return;
    if (pr.base.ref !== 'v4' || pr.head.ref !== branch || !branch?.startsWith('chore/lms-')) {
      throw new Error('Refusing PR outside the generated LMS feed branches');
    }
    if (pr.draft || pr.mergeable === false) throw new Error('Feed PR is draft or conflicting');
    if (pr.mergeable != null && pr.mergeable_state !== 'unknown') break;
    if (attempt === 2) throw new Error('Feed PR mergeability did not settle');
    await sleep(1000);
  }
  if (pr.mergeable_state === 'clean') {
    // Pin the inspected head; GitHub enforces branch protection and required checks.
    const { data } = await github.rest.pulls.merge({
      ...target, merge_method: 'squash', sha: pr.head.sha,
    });
    if (!data.merged) throw new Error(data.message || 'Feed PR was not merged');
    return;
  }
  // Pending checks/protection stay in force. GitHub merges once they are satisfied.
  await github.graphql(`
    mutation EnableAutoMerge($pullRequestId: ID!) {
      enablePullRequestAutoMerge(input: {
        pullRequestId: $pullRequestId, mergeMethod: SQUASH
      }) { pullRequest { number } }
    }
  `, { pullRequestId: pr.node_id });
};

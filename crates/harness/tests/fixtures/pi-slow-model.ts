// Load in an isolated Pi agent directory for real_acp_lifecycle.rs.
// Delay actual model requests AFTER completed tools, not their tool results
// or the ACP response. This recreates #296 with a genuinely active prompt.
export default function (pi) {
  let toolCompleted = false;
  pi.on('tool_execution_end', () => { toolCompleted = true; });
  pi.on('before_provider_request', async (_event, ctx) => {
    if (!toolCompleted) return;
    toolCompleted = false;
    const ms = Number(process.env.ACP_TEST_MODEL_DELAY_MS || 35000);
    process.stderr.write(`ACP_TEST: delaying post-tool model request by ${ms}ms\n`);
    await new Promise((resolve) => {
      const finish = () => {
        clearTimeout(timer);
        ctx.signal?.removeEventListener('abort', finish);
        resolve();
      };
      const timer = setTimeout(finish, ms);
      ctx.signal?.addEventListener('abort', finish, { once: true });
      if (ctx.signal?.aborted) finish();
    });
  });
}

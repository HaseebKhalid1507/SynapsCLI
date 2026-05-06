"""
Synaps — Terminal-native AI agent runtime for Harbor/Terminal-Bench.

Installed agent integration. Uses `synaps chat` in headless mode
with full tool execution (bash, file ops, etc).

Usage:
    harbor run -d terminal-bench/terminal-bench-2 \
        --agent-import-path synaps_agent:SynapsAgent \
        -m anthropic/claude-sonnet-4-6
"""

import shlex
from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext


class SynapsAgent(BaseInstalledAgent):
    """Synaps — terminal-native AI agent runtime."""

    @staticmethod
    def name() -> str:
        return "synaps"

    def version(self) -> str | None:
        return "0.1.4"

    async def install(self, environment: BaseEnvironment) -> None:
        """Install synaps from pre-built binary."""
        # Ensure curl and xz are available
        await self.exec_as_root(
            environment,
            command="apt-get update -qq && apt-get install -y -qq curl xz-utils > /dev/null 2>&1",
        )

        # Download pre-built binary — no compilation needed
        await self.exec_as_root(
            environment,
            command=(
                "curl -fsSL https://github.com/HaseebKhalid1507/SynapsCLI/releases/latest/download/synaps-x86_64-unknown-linux-gnu.tar.xz "
                "-o /tmp/synaps.tar.xz "
                "&& tar xJf /tmp/synaps.tar.xz -C /usr/local/bin/ --strip-components=1 "
                "&& chmod +x /usr/local/bin/synaps "
                "&& rm /tmp/synaps.tar.xz"
            ),
        )

        # Copy OAuth auth token if SYNAPS_AUTH_JSON env var is set
        await self.exec_as_agent(
            environment,
            command=(
                'mkdir -p ~/.synaps-cli && '
                'if [ -n "$SYNAPS_AUTH_JSON" ]; then '
                '  echo "$SYNAPS_AUTH_JSON" > ~/.synaps-cli/auth.json && '
                '  chmod 600 ~/.synaps-cli/auth.json && '
                '  echo "OAuth auth configured"; '
                'fi'
            ),
        )

    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        """
        Run synaps in headless chat mode.
        
        Pipes the instruction into `synaps chat` which has full tool access
        (bash, file read/write/edit, etc.) via the built-in tool suite.
        """
        # synaps chat reads from stdin, uses tools, outputs to stdout
        # --no-extensions to avoid plugin discovery overhead in benchmark containers
        await self.exec_as_agent(
            environment,
            command=f"echo {shlex.quote(instruction)} | synaps chat",
        )

    def populate_context_post_run(self, context: AgentContext) -> None:
        """
        Synaps outputs results to stdout during execution.
        The exec_as_agent call captures this automatically.
        """
        pass

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
        """Install synaps via the shell installer from GitHub Releases."""
        # Install Rust toolchain (needed for cargo install fallback)
        await self.exec_as_root(
            environment,
            command="apt-get update && apt-get install -y curl build-essential pkg-config libssl-dev",
        )

        # Install synaps from the shell installer (pre-built binary)
        await self.exec_as_agent(
            environment,
            command=(
                "curl -fsSL https://github.com/HaseebKhalid1507/SynapsCLI/releases/latest/download/synaps-installer.sh | sh "
                "|| cargo install synaps"  # fallback to cargo if installer fails
            ),
        )

        # Verify installation
        await self.exec_as_agent(
            environment,
            command="synaps --help",
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
            command=f"echo {shlex.quote(instruction)} | synaps chat --no-extensions",
        )

    def populate_context_post_run(self, context: AgentContext) -> None:
        """
        Synaps outputs results to stdout during execution.
        The exec_as_agent call captures this automatically.
        """
        pass

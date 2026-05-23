"""Harbor adapter for the little-coder agent.

Installs Node.js + little-coder into the bench task container and runs
it non-interactively against the task instruction.

little-coder wraps the pi coding agent CLI with extensions and skills.
Non-interactive mode: `little-coder --provider llamacpp --model ID -p "prompt"`.

Usage (from the itsy repo root):

    PYTHONPATH=$PWD/.agents/skills/terminal-bench-2 \\
    uv run --with harbor harbor run \\
        --dataset terminal-bench@2.0 \\
        --agent-import-path little_coder_agent:LittleCoderAgent \\
        --model unsloth/Qwen3.6-35B-A3B-GGUF:IQ2_XXS \\
        --include-task-name fix-git --n-attempts 3 --n-concurrent 1
"""

from __future__ import annotations

import os
import shlex

from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext


class LittleCoderAgent(BaseInstalledAgent):
    """little-coder — pi-based coding agent with extensions for small LLMs."""

    SUPPORTS_ATIF: bool = False
    SUPPORTS_WINDOWS: bool = False

    _OUTPUT_FILENAME = "little-coder.txt"

    @staticmethod
    def name() -> str:
        return "little-coder"

    def get_version_command(self) -> str | None:
        return "little-coder --version 2>&1 | head -1 || true"

    def parse_version(self, stdout: str) -> str:
        return stdout.strip() or "unknown"

    async def install(self, environment: BaseEnvironment) -> None:
        """Install Node.js + little-coder inside the bench container."""
        await self.exec_as_root(
            environment,
            command=(
                "set -e; "
                "if command -v apt-get >/dev/null 2>&1; then "
                "  DEBIAN_FRONTEND=noninteractive apt-get update -qq >/dev/null 2>&1 && "
                "  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends "
                "    ca-certificates curl git >/dev/null 2>&1 || true; "
                "elif command -v apk >/dev/null 2>&1; then "
                "  apk add --no-cache ca-certificates curl git >/dev/null 2>&1 || true; "
                "fi; "
                "if ! command -v node >/dev/null 2>&1; then "
                "  if command -v apt-get >/dev/null 2>&1; then "
                "    curl -fsSL https://deb.nodesource.com/setup_22.x | bash - >/dev/null 2>&1 && "
                "    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq nodejs >/dev/null 2>&1; "
                "  elif command -v apk >/dev/null 2>&1; then "
                "    apk add --no-cache nodejs npm >/dev/null 2>&1; "
                "  fi; "
                "fi; "
                "npm install -g little-coder >/dev/null 2>&1"
            ),
        )

    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        base_url = os.environ.get("LLAMACPP_BASE_URL", "http://10.0.2.2:8000/v1")
        model = self.model_name or os.environ.get("LITTLE_CODER_MODEL", "")
        if not model:
            raise ValueError("Model name required. Pass --model or set LITTLE_CODER_MODEL.")

        # little-coder expects provider/model-id format for llamacpp
        full_model = f"llamacpp/{model}" if "/" not in model else model
        escaped = shlex.quote(instruction)

        cmd = (
            "mkdir -p /logs/agent && "
            f"LLAMACPP_API_KEY=noop "
            f"LLAMACPP_BASE_URL={shlex.quote(base_url)} "
            f"PI_OFFLINE=1 "
            f"little-coder "
            f"--provider llamacpp "
            f"--model {shlex.quote(full_model)} "
            f"--print {escaped} "
            f"2>&1 </dev/null | tee /logs/agent/{self._OUTPUT_FILENAME}"
        )
        await self.exec_as_agent(environment, command=cmd)

    def populate_context_post_run(self, context: AgentContext) -> None:
        out = self.logs_dir / self._OUTPUT_FILENAME
        if out.exists():
            context.metadata = context.metadata or {}
            context.metadata["agent_log"] = out.read_text(errors="replace")[-8000:]

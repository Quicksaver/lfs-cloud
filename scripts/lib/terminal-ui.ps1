Set-StrictMode -Version Latest

$script:UI_PREFIX = '[script]'
$script:LIVE_REGION_ENABLED = $false
$script:LIVE_RENDER_MODE = 'section_task'
$script:LIVE_SECTION_MESSAGE = ''
$script:LIVE_SECTION_STATE = 'running'
$script:LIVE_TASK_MESSAGE = ''
$script:LIVE_TASK_STATE = 'running'
$script:LIVE_REGION_LINES = 0
$script:LIVE_STDOUT_MAX_LINES = 5
$script:LIVE_STDOUT_LINES = [System.Collections.Generic.List[string]]::new()
$script:LIVE_OUTPUT_TAIL_MAX_LINES = 40
$script:LIVE_OUTPUT_TAIL_LINES = [System.Collections.Generic.List[string]]::new()
$script:UI_TERMINAL_COLUMNS = 80

$script:RED = ''
$script:GREEN = ''
$script:YELLOW = ''
$script:BLUE = ''
$script:MAGENTA = ''
$script:GRAY = ''
$script:BOLD = ''
$script:RESET = ''

function ui_set_prefix {
    param([string]$Prefix)
    $script:UI_PREFIX = $Prefix
}

function ui_set_render_mode {
    param([string]$Mode)
    switch ($Mode) {
        'section_task' { $script:LIVE_RENDER_MODE = $Mode }
        'task_only' { $script:LIVE_RENDER_MODE = $Mode }
        default { throw "Unknown live render mode: $Mode" }
    }
}

function ui_refresh_terminal_columns {
    $columns = 0

    if ($Host -and $Host.UI -and $Host.UI.RawUI) {
        try {
            $columns = [int]$Host.UI.RawUI.WindowSize.Width
        }
        catch {
            $columns = 0
        }
    }

    if ($columns -le 0 -and $env:COLUMNS -match '^[0-9]+$') {
        $columns = [int]$env:COLUMNS
    }

    if ($columns -le 0) {
        $columns = 80
    }

    $script:UI_TERMINAL_COLUMNS = $columns
}

function ui_terminal_columns {
    return $script:UI_TERMINAL_COLUMNS
}

function ui_strip_ansi {
    param([string]$Text)

    if ($null -eq $Text) {
        return ''
    }

    $escapeChar = [string][char]0x1B
    $output = $Text

    # OSC: ESC ] ... (BEL or ESC \)
    $oscPattern = [regex]::Escape($escapeChar) + '\][^\x07\x1B]*(?:\x07|' + [regex]::Escape($escapeChar) + '\\)'
    $output = [regex]::Replace($output, $oscPattern, '')

    # CSI: ESC [ ... final-byte
    $output = [regex]::Replace($output, [regex]::Escape($escapeChar) + '\[[0-9;?]*[ -/]*[@-~]', '')

    # Any leftover ESC-prefixed sequence
    $output = [regex]::Replace($output, [regex]::Escape($escapeChar) + '[\x20-\x2F]*[\x30-\x7E]?', '')

    # Remove remaining control chars except tab; normalize tab to a single space
    $output = $output -replace "`t", ' '
    $output = [regex]::Replace($output, '[\x00-\x08\x0B-\x1F\x7F]', '')

    return $output
}

function ui_sanitize_live_line {
    param([string]$Text)

    if ($null -eq $Text) {
        return ''
    }

    $escapeChar = [string][char]0x1B
    $output = $Text

    # Preserve SGR color/style sequences (ESC[...m) for live colored output.
    $sgrTokens = [System.Collections.Generic.List[string]]::new()
    $output = [regex]::Replace($output, [regex]::Escape($escapeChar) + '\[[0-9;]*m', {
            param($match)
            $index = $sgrTokens.Count
            [void]$sgrTokens.Add($match.Value)
            return "__LFSCLOUD_SGR_${index}__"
        })

    # Strip remaining control/escape sequences.
    $output = ui_strip_ansi $output

    # Restore preserved SGR sequences.
    for ($i = 0; $i -lt $sgrTokens.Count; $i++) {
        $output = $output.Replace("__LFSCLOUD_SGR_${i}__", $sgrTokens[$i])
    }

    # Some upstream paths may strip ESC but leave SGR literals like "[1m".
    # Rehydrate those back to ANSI so colors render in live output.
    $output = ui_rehydrate_sgr_literals $output

    return $output
}

function ui_rehydrate_sgr_literals {
    param([string]$Text)

    if ([string]::IsNullOrEmpty($Text)) {
        return ''
    }

    $escapeChar = [string][char]0x1B
    return [regex]::Replace($Text, '(?<!\x1B)\[(?:\d{1,3}(?:;\d{1,3})*)m', {
            param($match)
            return "$escapeChar$($match.Value)"
        })
}

function ui_init {
    if ($env:NO_COLOR) {
        $script:RED = ''
        $script:GREEN = ''
        $script:YELLOW = ''
        $script:BLUE = ''
        $script:MAGENTA = ''
        $script:GRAY = ''
        $script:BOLD = ''
        $script:RESET = ''
    }
    elseif ($env:FORCE_COLOR -or $env:CLICOLOR_FORCE -or ($Host.UI -and $Host.UI.SupportsVirtualTerminal)) {
        $esc = [char]0x1B
        $script:RED = "${esc}[0;31m"
        $script:GREEN = "${esc}[0;32m"
        $script:YELLOW = "${esc}[0;33m"
        $script:BLUE = "${esc}[0;34m"
        $script:MAGENTA = "${esc}[0;35m"
        $script:GRAY = "${esc}[0;90m"
        $script:BOLD = "${esc}[1m"
        $script:RESET = "${esc}[0m"
    }
    else {
        $script:RED = ''
        $script:GREEN = ''
        $script:YELLOW = ''
        $script:BLUE = ''
        $script:MAGENTA = ''
        $script:GRAY = ''
        $script:BOLD = ''
        $script:RESET = ''
    }

    $supportsVt = $false
    if ($Host -and $Host.UI) {
        try {
            $supportsVt = [bool]$Host.UI.SupportsVirtualTerminal
        }
        catch {
            $supportsVt = $false
        }
    }

    $disableLiveRegion = $false
    if ($env:LFS_CLOUD_LIVE_REGION_DISABLE) {
        switch -Regex ($env:LFS_CLOUD_LIVE_REGION_DISABLE.Trim().ToLowerInvariant()) {
            '^(1|true|yes|on)$' { $disableLiveRegion = $true; break }
            '^(0|false|no|off)$' { $disableLiveRegion = $false; break }
            default { $disableLiveRegion = $true }
        }
    }

    if ($disableLiveRegion) {
        $script:LIVE_REGION_ENABLED = $false
    }
    elseif ($env:LFS_CLOUD_LIVE_REGION_FORCE) {
        $script:LIVE_REGION_ENABLED = $true
    }
    elseif (-not $env:NO_COLOR -and ($env:FORCE_COLOR -or $env:CLICOLOR_FORCE)) {
        $script:LIVE_REGION_ENABLED = $true
    }
    else {
        $script:LIVE_REGION_ENABLED = $supportsVt
    }

    ui_refresh_terminal_columns
}

function ui_fit_live_status_message {
    param([string]$Message)

    $maxLen = (ui_terminal_columns) - 24
    if ($maxLen -lt 20) {
        $maxLen = 20
    }

    if ($Message.Length -gt $maxLen) {
        return $Message.Substring(0, $maxLen - 3) + '...'
    }

    return $Message
}

function ui_fit_live_stream_line {
    param([string]$Line)

    $lineText = $Line

    $maxLen = (ui_terminal_columns) - 6
    if ($maxLen -lt 20) {
        $maxLen = 20
    }

    return ui_clip_ansi_visible -Text $lineText -MaxVisibleLength $maxLen
}

function ui_clip_ansi_visible {
    param(
        [string]$Text,
        [int]$MaxVisibleLength
    )

    if ([string]::IsNullOrEmpty($Text) -or $MaxVisibleLength -le 0) {
        return ''
    }

    $builder = New-Object System.Text.StringBuilder
    $index = 0
    $visible = 0
    $wasClipped = $false
    $length = $Text.Length

    while ($index -lt $length) {
        $char = $Text[$index]
        if ($char -eq [char]0x1B) {
            $next = $index + 1
            if ($next -lt $length -and $Text[$next] -eq '[') {
                $seqStart = $index
                $index += 2
                while ($index -lt $length) {
                    $final = [int][char]$Text[$index]
                    if ($final -ge 64 -and $final -le 126) {
                        $index++
                        break
                    }

                    $index++
                }

                [void]$builder.Append($Text.Substring($seqStart, $index - $seqStart))
                continue
            }

            if ($next -lt $length -and $Text[$next] -eq ']') {
                $index += 2
                while ($index -lt $length) {
                    if ($Text[$index] -eq [char]0x07) {
                        $index++
                        break
                    }

                    if ($Text[$index] -eq [char]0x1B -and ($index + 1) -lt $length -and $Text[$index + 1] -eq '\\') {
                        $index += 2
                        break
                    }

                    $index++
                }

                continue
            }

            $index++
            continue
        }

        if ($visible -ge $MaxVisibleLength) {
            $wasClipped = $true
            break
        }

        [void]$builder.Append($char)
        $visible++
        $index++
    }

    $result = $builder.ToString()
    if ($wasClipped -and $Text.Contains([char]0x1B)) {
        return "$result$($script:RESET)"
    }

    return $result
}

function ui_format_line {
    param(
        [string]$State,
        [string]$Message,
        [switch]$Child
    )

    $symbol = ''
    $label = ''
    $color = ''

    switch ($State) {
        'running' { $symbol = '>'; $label = 'RUN '; $color = $script:BLUE }
        'pass' { $symbol = '+'; $label = 'PASS'; $color = $script:GREEN }
        'fail' { $symbol = 'x'; $label = 'FAIL'; $color = $script:RED }
        'warn' { $symbol = '!'; $label = 'WARN'; $color = $script:YELLOW }
        'info' { $symbol = 'i'; $label = 'INFO'; $color = $script:BLUE }
        'skip' { $symbol = 'o'; $label = 'SKIP'; $color = $script:MAGENTA }
        default { $symbol = '>'; $label = 'RUN '; $color = $script:BLUE }
    }

    $prefix = $script:UI_PREFIX
    if ($script:GRAY) {
        $prefix = "$($script:GRAY)$($script:UI_PREFIX)$($script:RESET)"
    }

    $branch = ''
    if ($Child) {
        if ($script:GRAY) {
            $branch = "$($script:GRAY)`-$($script:RESET) "
        }
        else {
            $branch = '`- '
        }
    }

    if ($color) {
        return "$prefix $branch$color$symbol $label$($script:RESET) $Message"
    }

    return "$prefix $branch$symbol $label $Message"
}

function ui_live_stdout_is_visible {
    if (-not $script:LIVE_TASK_MESSAGE) {
        return $false
    }

    return $true
}

function ui_clear_live_region {
    if (-not $script:LIVE_REGION_ENABLED -or $script:LIVE_REGION_LINES -le 0) {
        return
    }

    $esc = [char]0x1B
    for ($i = 0; $i -lt $script:LIVE_REGION_LINES; $i++) {
        [Console]::Write("$esc[1A")
        [Console]::Write("$esc[2K")
        [Console]::Write("`r")
    }
    $script:LIVE_REGION_LINES = 0
}

function ui_render_live_region {
    if (-not $script:LIVE_REGION_ENABLED) {
        return
    }

    ui_refresh_terminal_columns

    $lines = [System.Collections.Generic.List[string]]::new()

    if ($script:LIVE_RENDER_MODE -eq 'task_only') {
        if ($script:LIVE_TASK_MESSAGE) {
            $taskMessage = ui_fit_live_status_message $script:LIVE_TASK_MESSAGE
            $lines.Add((ui_format_line -State $script:LIVE_TASK_STATE -Message $taskMessage))
        }
    }
    else {
        if ($script:LIVE_SECTION_MESSAGE) {
            $sectionMessage = ui_fit_live_status_message $script:LIVE_SECTION_MESSAGE
            $lines.Add((ui_format_line -State $script:LIVE_SECTION_STATE -Message $sectionMessage))
        }

        if ($script:LIVE_TASK_MESSAGE) {
            $taskMessage = ui_fit_live_status_message $script:LIVE_TASK_MESSAGE
            $lines.Add((ui_format_line -State $script:LIVE_TASK_STATE -Message $taskMessage -Child))
        }
    }

    if (ui_live_stdout_is_visible) {
        foreach ($line in $script:LIVE_STDOUT_LINES) {
            $fittedLine = ui_fit_live_stream_line $line
            if ($script:LIVE_RENDER_MODE -eq 'task_only') {
                $lines.Add("  | $fittedLine")
            }
            else {
                $lines.Add("   | $fittedLine")
            }
        }
    }

    foreach ($line in $lines) {
        Write-Host $line
    }

    $script:LIVE_REGION_LINES = $lines.Count
}

function ui_live_stdout_reset {
    $script:LIVE_STDOUT_LINES.Clear()
    $script:LIVE_OUTPUT_TAIL_LINES.Clear()
    ui_clear_live_region
    ui_render_live_region
}

function ui_live_stdout_append_line {
    param([string]$RawLine)

    if ($null -eq $RawLine) {
        return
    }

    $normalized = $RawLine -replace "`r", "`n"
    $parts = $normalized -split "`n"
    foreach ($part in $parts) {
        $livePart = ui_sanitize_live_line $part
        $cleanForCheck = ui_strip_ansi $livePart
        if (-not $cleanForCheck.Trim()) {
            continue
        }

        if ($script:LIVE_OUTPUT_TAIL_LINES.Count -lt $script:LIVE_OUTPUT_TAIL_MAX_LINES) {
            $script:LIVE_OUTPUT_TAIL_LINES.Add($livePart)
        }
        else {
            $script:LIVE_OUTPUT_TAIL_LINES.RemoveAt(0)
            $script:LIVE_OUTPUT_TAIL_LINES.Add($livePart)
        }

        if ($script:LIVE_STDOUT_LINES.Count -lt $script:LIVE_STDOUT_MAX_LINES) {
            $script:LIVE_STDOUT_LINES.Add($livePart)
        }
        else {
            $script:LIVE_STDOUT_LINES.RemoveAt(0)
            $script:LIVE_STDOUT_LINES.Add($livePart)
        }
    }

    ui_clear_live_region
    ui_render_live_region
}

function ui_get_live_stdout_lines {
    return @($script:LIVE_STDOUT_LINES)
}

function ui_get_live_output_tail_lines {
    return @($script:LIVE_OUTPUT_TAIL_LINES)
}

function ui_set_live_section_running {
    param([string]$Message)

    $script:LIVE_SECTION_MESSAGE = $Message
    $script:LIVE_SECTION_STATE = 'running'
    ui_clear_live_region
    ui_render_live_region
}

function ui_set_live_task_state {
    param(
        [string]$State,
        [string]$Message
    )

    $script:LIVE_TASK_STATE = $State
    $script:LIVE_TASK_MESSAGE = $Message
    ui_clear_live_region
    ui_render_live_region
}

function ui_clear_live_task {
    $script:LIVE_TASK_MESSAGE = ''
    $script:LIVE_TASK_STATE = 'running'
    $script:LIVE_STDOUT_LINES.Clear()
    $script:LIVE_OUTPUT_TAIL_LINES.Clear()
    ui_clear_live_region
    ui_render_live_region
}

function ui_clear_live_state {
    $script:LIVE_SECTION_MESSAGE = ''
    $script:LIVE_SECTION_STATE = 'running'
    $script:LIVE_TASK_MESSAGE = ''
    $script:LIVE_TASK_STATE = 'running'
    $script:LIVE_STDOUT_LINES.Clear()
    $script:LIVE_OUTPUT_TAIL_LINES.Clear()
    ui_clear_live_region
}

function ui_pause_live_region_for_command_output {
    if (-not $script:LIVE_REGION_ENABLED) {
        return
    }

    ui_clear_live_region
}

function ui_resume_live_region_after_command_output {
    if (-not $script:LIVE_REGION_ENABLED) {
        return
    }

    ui_render_live_region
}

function ui_log_persistent {
    param(
        [string]$State,
        [string]$Message
    )

    ui_clear_live_region
    Write-Host (ui_format_line -State $State -Message $Message)
    ui_render_live_region
}

function ui_log_persistent_raw {
    param(
        [string]$Message,
        [string]$FallbackColor = ''
    )

    ui_pause_live_region_for_command_output

    $renderMessage = ui_rehydrate_sgr_literals $Message

    if ($renderMessage.Contains([char]0x1B)) {
        Write-Host $renderMessage
    }
    elseif ($FallbackColor) {
        Write-Host "$FallbackColor$renderMessage$($script:RESET)"
    }
    else {
        Write-Host $renderMessage
    }

    ui_resume_live_region_after_command_output
}

function ui_log_persistent_raw_batch {
    param(
        [string]$Message,
        [string]$FallbackColor = ''
    )

    if (-not $Message) {
        return
    }

    $lines = $Message -split "`r?`n"
    foreach ($line in $lines) {
        if ($line) {
            ui_log_persistent_raw -Message $line -FallbackColor $FallbackColor
        }
    }
}

function ui_force_color_env {
    Remove-Item Env:NO_COLOR -ErrorAction SilentlyContinue
    if (-not $env:TERM) {
        $env:TERM = 'xterm-256color'
    }

    $env:CLICOLOR = '1'
    $env:CLICOLOR_FORCE = '1'
    $env:FORCE_COLOR = '1'
    $env:CARGO_TERM_COLOR = 'always'
    $env:RUST_LOG_STYLE = 'always'
}

function ui_run_with_live_stdout {
    param(
        [Parameter(Mandatory = $true, Position = 0)] [string]$Command,
        [Parameter(Position = 1)] [string[]]$Arguments
    )

    if (-not $script:LIVE_REGION_ENABLED) {
        try {
            & $Command @Arguments
            if ($null -eq $LASTEXITCODE) {
                return $true
            }

            return ($LASTEXITCODE -eq 0)
        }
        catch {
            return $false
        }
    }

    $workingDirectory = (Get-Location).Path
    $job = $null

    ui_live_stdout_reset

    $jobExitCode = $null
    $drainJob = {
        param([ref] $ExitCode)

        foreach ($item in @(Receive-Job -Job $job -ErrorAction SilentlyContinue)) {
            $exitCodeProperty = $item.PSObject.Properties['LfsCloudNativeExitCode']
            if ($null -ne $exitCodeProperty) {
                $ExitCode.Value = [int] $exitCodeProperty.Value
                continue
            }

            foreach ($line in ([string] $item -replace "`r", '' -split "`n")) {
                if (-not [string]::IsNullOrWhiteSpace($line)) {
                    ui_live_stdout_append_line $line
                }
            }
        }
    }

    try {
        $job = Start-Job -ScriptBlock {
            param(
                [string]$InnerCommand,
                [string[]]$InnerArguments,
                [string]$InnerWorkingDirectory
            )

            $ErrorActionPreference = 'Continue'
            $PSNativeCommandUseErrorActionPreference = $false
            $exitCode = 1
            try {
                Set-Location -Path $InnerWorkingDirectory
                & $InnerCommand @InnerArguments 2>&1 |
                    ForEach-Object { $_.ToString() }
                $exitCode = if ($null -eq $LASTEXITCODE) { 0 } else { [int] $LASTEXITCODE }
            }
            catch {
                $_ | Out-String
            }

            [pscustomobject] @{ LfsCloudNativeExitCode = $exitCode }
        } -ArgumentList @($Command, $Arguments, $workingDirectory)

        while ($job.State -eq 'Running' -or $job.State -eq 'NotStarted') {
            & $drainJob ([ref] $jobExitCode)
            Start-Sleep -Milliseconds 40
        }

        & $drainJob ([ref] $jobExitCode)
        if ($null -eq $jobExitCode) {
            $exitCode = 1
        }
        else {
            $exitCode = [int]$jobExitCode
        }
    }
    finally {
        if ($job) {
            Remove-Job -Job $job -Force -ErrorAction SilentlyContinue
        }
    }

    if ($script:LIVE_STDOUT_LINES.Count -gt 0) {
        ui_clear_live_region
        ui_render_live_region
    }

    return ($exitCode -eq 0)
}

function ui_finalize {
    ui_clear_live_state
}

function info {
    param([string]$Message)
    ui_log_persistent -State 'info' -Message $Message
}

function warn {
    param([string]$Message)
    ui_log_persistent -State 'warn' -Message $Message
}

function pass {
    param([string]$Message)
    ui_log_persistent -State 'pass' -Message $Message
}

function fail {
    param([string]$Message)
    ui_log_persistent -State 'fail' -Message $Message
}

function skip {
    param([string]$Message)
    ui_log_persistent -State 'skip' -Message $Message
}

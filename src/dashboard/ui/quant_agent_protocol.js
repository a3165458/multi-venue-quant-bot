(function (root, factory) {
    var api = factory();
    if (typeof module === 'object' && module.exports) module.exports = api;
    if (root) root.QuantAgentProtocol = api;
})(typeof globalThis !== 'undefined' ? globalThis : this, function () {
    'use strict';

    function decodeText(value) {
        return String(value || '')
            .replace(/&quot;/g, '"').replace(/&#39;|&apos;/g, "'")
            .replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&amp;/g, '&');
    }

    function typedValue(raw, isString) {
        var text = decodeText(raw).trim();
        if (isString) return text;
        try { return JSON.parse(text); } catch (e) {
            if (text === 'true') return true;
            if (text === 'false') return false;
            var number = Number(text);
            return Number.isFinite(number) ? number : text;
        }
    }

    function parseToolProtocol(content) {
        var source = String(content || '');
        var toolCalls = [];
        var dsml = '(?:\\|\\||｜｜)DSML(?:\\|\\||｜｜)';
        var invokeRe = new RegExp('<' + dsml + 'invoke\\s+name="([^"]+)"[^>]*>([\\s\\S]*?)<\\/' + dsml + 'invoke>', 'g');
        var invoke;
        var invokeIndex = 0;
        while ((invoke = invokeRe.exec(source)) !== null) {
            var args = {};
            var paramRe = new RegExp('<' + dsml + 'parameter\\s+name="([^"]+)"(?:\\s+string="(true|false)")?[^>]*>([\\s\\S]*?)<\\/' + dsml + 'parameter>', 'g');
            var param;
            while ((param = paramRe.exec(invoke[2])) !== null) {
                args[param[1]] = typedValue(param[3], param[2] !== 'false');
            }
            toolCalls.push({ id: 'dsml_' + invokeIndex++, name: invoke[1], arguments: args });
        }

        var lines = source.split(/\r?\n/);
        var finalText = source;
        lines.forEach(function (line, idx) {
            var match = line.match(/^\s*TOOL_CALL\s+(\{.*\})\s*$/);
            if (match) {
                try {
                    var parsed = JSON.parse(match[1]);
                    toolCalls.push({ id: 'text_' + idx, name: parsed.name, arguments: parsed.arguments || {} });
                } catch (e) { /* malformed fallback is left visible */ }
            }
            var finalMatch = line.match(/^\s*FINAL\s+([\s\S]*)$/);
            if (finalMatch) finalText = finalMatch[1];
        });

        if (toolCalls.length) {
            var toolCallsBlock = new RegExp('<' + dsml + 'tool_calls[^>]*>[\\s\\S]*?<\\/' + dsml + 'tool_calls>', 'g');
            finalText = source.replace(toolCallsBlock, '').replace(/^\s*TOOL_CALL\s+\{.*\}\s*$/gm, '').trim();
        }
        return { role: 'assistant', content: finalText.trim(), tool_calls: toolCalls };
    }

    return { parseToolProtocol: parseToolProtocol };
});

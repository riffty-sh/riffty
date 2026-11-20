package sh.riffty.java;

import java.io.*;
import java.net.StandardProtocolFamily;
import java.net.UnixDomainSocketAddress;
import java.nio.channels.Channels;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Instant;
import java.util.*;
import java.util.concurrent.Executors;

/**
 * Entrypoint for `riffty-java` (JDK 25 LTS).
 * - Default mode (Milestone 2): newline-delimited JSON-RPC v1 over stdin/stdout.
 * - Daemon mode (Milestone 3): `--daemon <unix_socket_path>` using Virtual Threads (JEP 491)
 *   and ScopedValue (JEP 506) for sub-100ms warm interactive analysis.
 */
public final class RifftyJavaMain {

    public record RequestContext(String requestId, Path workspaceRoot, Instant startedAt) {}

    public static final ScopedValue<RequestContext> CURRENT = ScopedValue.newInstance();

    private static final JavaAnalyzerEngine ENGINE = new JavaAnalyzerEngine();

    public static void main(String[] args) throws Exception {
        if (args.length >= 2 && "--daemon".equals(args[0])) {
            runDaemon(Path.of(args[1]));
            return;
        }
        runStdio();
    }

    private static void runStdio() throws Exception {
        try (BufferedReader reader = new BufferedReader(new InputStreamReader(System.in, StandardCharsets.UTF_8));
             BufferedWriter writer = new BufferedWriter(new OutputStreamWriter(System.out, StandardCharsets.UTF_8))) {
            String line;
            while ((line = reader.readLine()) != null) {
                if (line.isBlank()) continue;
                String response = handleRequestLine(line, "stdio");
                writer.write(response);
                writer.newLine();
                writer.flush();
            }
        }
    }

    private static void runDaemon(Path socketPath) throws Exception {
        Files.deleteIfExists(socketPath);
        if (socketPath.getParent() != null) {
            Files.createDirectories(socketPath.getParent());
        }
        UnixDomainSocketAddress addr = UnixDomainSocketAddress.of(socketPath);
        try (ServerSocketChannel server = ServerSocketChannel.open(StandardProtocolFamily.UNIX);
             var executor = Executors.newVirtualThreadPerTaskExecutor()) {
            server.bind(addr);
            System.err.println("riffty-java daemon listening on " + socketPath);
            while (true) {
                SocketChannel client = server.accept();
                executor.submit(() -> handleDaemonClient(client));
            }
        }
    }

    private static void handleDaemonClient(SocketChannel client) {
        try (client;
             BufferedReader reader = new BufferedReader(new InputStreamReader(Channels.newInputStream(client), StandardCharsets.UTF_8));
             BufferedWriter writer = new BufferedWriter(new OutputStreamWriter(Channels.newOutputStream(client), StandardCharsets.UTF_8))) {
            String line;
            while ((line = reader.readLine()) != null) {
                if (line.isBlank()) continue;
                String resp = handleRequestLine(line, "warm-daemon");
                writer.write(resp);
                writer.newLine();
                writer.flush();
            }
        } catch (Throwable e) {
            System.err.println("riffty-java daemon client error: " + e.getMessage());
        }
    }

    @SuppressWarnings("unchecked")
    private static String handleRequestLine(String line, String modeTag) {
        Map<String, Object> req = MiniJson.parseObject(line);
        String id = String.valueOf(req.getOrDefault("id", "pass-0"));
        String wsRootStr = (String) req.get("workspace_root");
        Path wsRoot = wsRootStr != null ? Path.of(wsRootStr) : Path.of(".");

        RequestContext ctx = new RequestContext(id, wsRoot, Instant.now());
        try {
            return ScopedValue.where(CURRENT, ctx).call(() -> {
                List<String> classpath = new ArrayList<>();
                if (req.get("classpath") instanceof List<?> cpList) {
                    for (Object item : cpList) classpath.add(String.valueOf(item));
                }

                List<JavaAnalyzerEngine.FileInput> files = new ArrayList<>();
                if (req.get("files") instanceof List<?> fileList) {
                    for (Object item : fileList) {
                        Map<String, Object> fm = (Map<String, Object>) item;
                        int fileId = ((Number) fm.get("file_id")).intValue();
                        String rel = String.valueOf(fm.get("relative_path"));
                        String src = String.valueOf(fm.get("source"));
                        files.add(new JavaAnalyzerEngine.FileInput(fileId, rel, src));
                    }
                }

                List<JavaAnalyzerEngine.ArchForbidRule> archRules = new ArrayList<>();
                if (req.get("architecture_forbid") instanceof List<?> ruleList) {
                    for (Object item : ruleList) {
                        Map<String, Object> rm = (Map<String, Object>) item;
                        String rId = String.valueOf(rm.get("id"));
                        String from = String.valueOf(rm.get("from"));
                        String msg = String.valueOf(rm.get("message"));
                        List<String> targets = new ArrayList<>();
                        if (rm.get("target") instanceof List<?> tList) {
                            for (Object t : tList) targets.add(String.valueOf(t));
                        }
                        archRules.add(new JavaAnalyzerEngine.ArchForbidRule(rId, from, targets, msg));
                    }
                }

                JavaAnalyzerEngine.AnalyzeResult result =
                    ENGINE.analyze(CURRENT.get().workspaceRoot(), classpath, files, archRules, modeTag);

                Map<String, Object> resp = new LinkedHashMap<>();
                resp.put("protocol", 1);
                resp.put("id", CURRENT.get().requestId());
                resp.put("provenance", result.provenance());
                resp.put("diagnostics", result.diagnostics());
                return MiniJson.stringify(resp);
            });
        } catch (Throwable e) {
            Map<String, Object> err = new LinkedHashMap<>();
            err.put("protocol", 1);
            err.put("id", id);
            err.put("error", e.getClass().getSimpleName() + ": " + e.getMessage());
            return MiniJson.stringify(err);
        }
    }
}


using System.Text;
using SessionHub.Core.Protocol;

namespace SessionHub.Core.Bridge;

/// <summary>Link state shown by the tray icon.</summary>
public abstract record BridgeHealth
{
    private BridgeHealth()
    {
    }

    /// <summary>A bridge answered with a valid <c>hello</c>.</summary>
    public sealed record Connected : BridgeHealth;

    /// <summary>The bridge is down; start number <paramref name="Attempt"/> in this run of failures comes after <paramref name="Next"/>.</summary>
    public sealed record Restarting(int Attempt, TimeSpan Next) : BridgeHealth;

    /// <summary><paramref name="Attempts"/> starts in a row failed (red icon). Restarts go on with backoff.</summary>
    public sealed record Failed(int Attempts) : BridgeHealth;
}

/// <summary>
/// Runs <c>hub bridge</c>, turns its lines into events and matches replies to requests. When the
/// bridge ends it is restarted after 1 s, doubling to 30 s. A start fails when the bridge does not
/// say hello, or says hello and ends within 30 s; three failed starts in a row are
/// <see cref="BridgeHealth.Failed"/>. A connected bridge that sends no line for 45 s is ended and
/// restarted the same way. The last snapshot is kept across restarts.
/// </summary>
/// <remarks>
/// <see cref="RequestAsync"/> never throws: every failure is a <see cref="Reply"/> with an error code:
/// <c>timeout</c>, <c>bridge-restarted</c> (the bridge ended first), <c>not-connected</c>,
/// <c>bad-request</c>, <c>too-long</c> (over 1 MiB) or <c>disposed</c>.
/// Events are raised through the <see cref="SynchronizationContext"/> given to the constructor. A
/// Snapshot or SendChanged handler that throws inline ends the current bridge, which is restarted.
/// </remarks>
public sealed class BridgeClient : IAsyncDisposable
{
    public const int ProtocolVersion = 1;
    public const int MaxLineBytes = 1_048_576;
    public const int FailedAfter = 3;
    public static readonly TimeSpan HelloTimeout = TimeSpan.FromSeconds(10);
    public static readonly TimeSpan StableAfter = TimeSpan.FromSeconds(30);
    public static readonly TimeSpan IdleTimeout = TimeSpan.FromSeconds(45);
    public static readonly TimeSpan MaxBackoff = TimeSpan.FromSeconds(30);
    public static readonly TimeSpan MaxTimeout = TimeSpan.FromDays(1);

    private readonly BridgeFactory factory;
    private readonly SynchronizationContext context;
    private readonly TimeProvider time;
    private readonly CancellationTokenSource stop = new();
    private readonly object gate = new();
    private readonly Dictionary<int, TaskCompletionSource<Reply>> pending = [];

    private Link? current;
    private bool disposed;
    private Task? supervisor;
    private int nextId;
    private int malformedLines;
    private int resumes;
    private volatile HubSnapshot? lastSnapshot;
    private volatile BridgeHealth? health;

    public BridgeClient(BridgeFactory factory, SynchronizationContext context, TimeProvider? time = null)
    {
        ArgumentNullException.ThrowIfNull(factory);
        ArgumentNullException.ThrowIfNull(context);
        this.factory = factory;
        this.context = context;
        this.time = time ?? TimeProvider.System;
    }

    public event Action<HubSnapshot>? Snapshot;

    public event Action<SendUpdate>? SendChanged;

    public event Action<BridgeHealth>? HealthChanged;

    /// <summary>The newest snapshot received, kept across restarts. Null before the first one.</summary>
    public HubSnapshot? LastSnapshot => lastSnapshot;

    /// <summary>The newest health raised. Null before the first start ends or succeeds.</summary>
    public BridgeHealth? Health => health;

    /// <summary>Lines from the bridge that were skipped: not valid events, or over 1 MiB.</summary>
    public int MalformedLines => Volatile.Read(ref malformedLines);

    /// <summary>
    /// The PC woke from sleep. The bridge was suspended too, so the silence that the 45 s watchdog
    /// may count across the sleep is not a hang: the wait running now gets one more
    /// <see cref="IdleTimeout"/> before the bridge is ended. Safe from any thread.
    /// </summary>
    public void NotifyResumed() => Interlocked.Increment(ref resumes);

    /// <summary>Starts the first bridge. Later calls do nothing.</summary>
    public void Start()
    {
        lock (gate)
        {
            if (disposed || supervisor is not null) return;
            supervisor = Task.Run(() => SuperviseAsync(stop.Token));
        }
    }

    /// <summary>
    /// Sends one request and waits for its reply. The client sets the request id; the id the caller
    /// put in <paramref name="request"/> is replaced. <paramref name="timeout"/> must be above zero
    /// and at most one day; it covers the wait for the write slot, the write and the reply. A request
    /// that timed out is never written. Never throws.
    /// </summary>
    public async Task<Reply> RequestAsync(BridgeRequest request, TimeSpan timeout)
    {
        var id = Interlocked.Increment(ref nextId);
        if (timeout <= TimeSpan.Zero || timeout > MaxTimeout)
            return Fail(id, "bad-request", "timeout must be above zero and at most one day");

        BridgeRequest? withId = request switch
        {
            HistoryRequest r => r with { Id = id },
            SearchRequest r => r with { Id = id },
            SendRequest r => r with { Id = id },
            StartDaemonRequest r => r with { Id = id },
            PingRequest r => r with { Id = id },
            AnswerRequest r => r with { Id = id },
            _ => null,
        };
        if (withId is null) return Fail(id, "bad-request", "request type has no reply");

        string line;
        try
        {
            line = Json.Encode(withId);
        }
        catch (Exception)
        {
            // Defensive: Encode writes lone surrogates as U+FFFD, so no known input throws here.
            return Fail(id, "bad-request", "request cannot be encoded");
        }

        if (Encoding.UTF8.GetByteCount(line) > MaxLineBytes)
            return Fail(id, "too-long", $"request is longer than {MaxLineBytes} bytes");

        var reply = new TaskCompletionSource<Reply>(TaskCreationOptions.RunContinuationsAsynchronously);
        Link link;
        lock (gate)
        {
            if (disposed) return Fail(id, "disposed", "bridge client is disposed");
            if (current is not { Connected: true } live) return Fail(id, "not-connected", "bridge is not connected");
            link = live;
            pending[id] = reply;
        }

        using var timer = new CancellationTokenSource(timeout, time);
        using var either = CancellationTokenSource.CreateLinkedTokenSource(timer.Token, link.Ended);
        try
        {
            await link.WriteAsync(line, either.Token).ConfigureAwait(false);
            return await reply.Task.WaitAsync(timer.Token).ConfigureAwait(false);
        }
        catch (Exception) when (timer.IsCancellationRequested)
        {
            return await SettleAsync(id, reply, "timeout", "no reply in time").ConfigureAwait(false);
        }
        catch (Exception)
        {
            // The bridge ended, or the write failed because it is ending; the supervisor restarts it.
            return await SettleAsync(id, reply, "bridge-restarted", "bridge ended before the reply")
                .ConfigureAwait(false);
        }
    }

    public async ValueTask DisposeAsync()
    {
        Task? running;
        lock (gate)
        {
            if (disposed) return;
            disposed = true;
            running = supervisor;
        }

        await stop.CancelAsync().ConfigureAwait(false);
        FailPending("disposed", "bridge client is disposed");
        if (running is not null)
        {
            try
            {
                await running.ConfigureAwait(false);
            }
            catch (OperationCanceledException)
            {
                // Expected on stop.
            }
        }

        stop.Dispose();
    }

    private async Task SuperviseAsync(CancellationToken ct)
    {
        var attempt = 0;
        var failedStarts = 0;
        while (!ct.IsCancellationRequested)
        {
            bool stable;
            try
            {
                stable = await RunOnceAsync(ct).ConfigureAwait(false);
            }
            catch (Exception)
            {
                // RunOnceAsync guards its own steps; anything left still only ends this bridge.
                stable = false;
            }

            if (ct.IsCancellationRequested) return;

            if (stable)
            {
                attempt = 0;
                failedStarts = 0;
            }
            else
            {
                failedStarts++;
            }

            attempt++;
            var delay = Backoff(attempt);
            Raise(failedStarts >= FailedAfter
                ? new BridgeHealth.Failed(failedStarts)
                : new BridgeHealth.Restarting(attempt, delay));
            try
            {
                await Task.Delay(delay, time, ct).ConfigureAwait(false);
            }
            catch (OperationCanceledException)
            {
                return;
            }
        }
    }

    /// <summary>Runs one bridge until it ends. True when it said hello and stayed up for 30 s.</summary>
    private async Task<bool> RunOnceAsync(CancellationToken ct)
    {
        IBridgeProcess process;
        try
        {
            process = factory();
        }
        catch (Exception)
        {
            return false;
        }

        var link = new Link(process);
        lock (gate) current = link;
        long? helloAt = null;
        long endedAt = 0;
        try
        {
            try
            {
                var first = await ReadAsync(process, HelloTimeout, ct).ConfigureAwait(false);
                if (first is null || !IsHello(first)) return false;

                helloAt = time.GetTimestamp();
                lock (gate) link.Connected = true;
                Raise(new BridgeHealth.Connected());

                // Watchdog: the bridge sends a line (a snapshot, or a heartbeat when nothing changed)
                // at least every 15 s, so 45 s of silence means it hangs. The timeout ends it like an
                // exit. Every line read restarts the wait, so a heartbeat needs no handling of its own.
                // A wait that a resume from sleep overlapped gets one more period on the same read.
                Task<string?>? read = null;
                while (true)
                {
                    read ??= StartRead(process, ct);
                    var resumesBefore = Volatile.Read(ref resumes);
                    string? line;
                    try
                    {
                        line = await read.WaitAsync(IdleTimeout, time, ct).ConfigureAwait(false);
                    }
                    catch (TimeoutException) when (Volatile.Read(ref resumes) != resumesBefore)
                    {
                        continue;
                    }

                    read = null;
                    if (line is null) break;
                    Handle(line);
                }
            }
            catch (Exception)
            {
                // Hello or idle timeout, read failure, a throwing handler or stop: this bridge is over.
            }

            // Taken before teardown, so a slow dispose never makes a short-lived bridge look stable.
            endedAt = time.GetTimestamp();
        }
        finally
        {
            lock (gate)
            {
                link.Connected = false;
                if (current == link) current = null;
            }

            // Fail requests first, and guard End on its own: a throwing cancellation callback must
            // never skip the process disposal below.
            FailPending(disposed ? "disposed" : "bridge-restarted", "bridge ended before the reply");
            try
            {
                link.End();
            }
            catch (Exception)
            {
                // Cancel runs every callback before it throws, so all waiters still end.
            }

            try
            {
                await Task.Run(process.Dispose, CancellationToken.None).ConfigureAwait(false);
            }
            catch (Exception)
            {
                // A failed dispose must never stop supervision. HubBridgeProcess guards its own kill.
            }
        }

        return helloAt is { } at && time.GetElapsedTime(at, endedAt) >= StableAfter;
    }

    // WaitAsync makes a stop or timeout return now even when the pipe read ignores the token. The
    // abandoned read ends when the process is disposed; its fault is observed here.
    private Task<string?> ReadAsync(IBridgeProcess process, TimeSpan timeout, CancellationToken ct) =>
        StartRead(process, ct).WaitAsync(timeout, time, ct);

    private static Task<string?> StartRead(IBridgeProcess process, CancellationToken ct)
    {
        var read = process.ReadLineAsync(ct);
        Observe(read);
        return read;
    }

    private static void Observe(Task task) =>
        _ = task.ContinueWith(
            static t => _ = t.Exception,
            CancellationToken.None,
            TaskContinuationOptions.OnlyOnFaulted | TaskContinuationOptions.ExecuteSynchronously,
            TaskScheduler.Default);

    // Any other first line, or another protocol version, is a failed start.
    private bool IsHello(string line) => Parse(line) is Hello { Protocol: ProtocolVersion };

    private void Handle(string line)
    {
        switch (Parse(line))
        {
            case SnapshotEvent s:
                lastSnapshot = s.Snapshot;
                Post(() => Snapshot?.Invoke(s.Snapshot));
                break;
            case SendUpdate u:
                Post(() => SendChanged?.Invoke(u));
                break;
            case Reply r:
                TaskCompletionSource<Reply>? waiter;
                lock (gate) pending.Remove(r.Id, out waiter);
                waiter?.TrySetResult(r);
                break;

            // heartbeat (liveness only), hello again and bridge error events need no action.
        }
    }

    /// <summary>One event, or null for a skipped line (counted in <see cref="MalformedLines"/>).</summary>
    private BridgeEvent? Parse(string line)
    {
        var ev = Encoding.UTF8.GetByteCount(line) > MaxLineBytes ? null : Json.ParseEvent(line);
        if (ev is null) Interlocked.Increment(ref malformedLines);
        return ev;
    }

    // Health must never end a bridge or the supervisor, even when a handler throws inline.
    private void Raise(BridgeHealth h)
    {
        health = h;
        try
        {
            Post(() => HealthChanged?.Invoke(h));
        }
        catch (Exception)
        {
            // The handler's problem; the health value above is still current.
        }
    }

    private void Post(Action raise) => context.Post(static s => ((Action)s!)(), raise);

    private void FailPending(string code, string message)
    {
        KeyValuePair<int, TaskCompletionSource<Reply>>[] waiting;
        lock (gate)
        {
            waiting = [.. pending];
            pending.Clear();
        }

        foreach (var (id, waiter) in waiting) waiter.TrySetResult(Fail(id, code, message));
    }

    /// <summary>
    /// Ends a request with <paramref name="code"/>, unless a reply or a restart took it from
    /// <see cref="pending"/> first: that one has set, or is about to set, the result, and it wins.
    /// </summary>
    private async Task<Reply> SettleAsync(int id, TaskCompletionSource<Reply> reply, string code, string message)
    {
        bool mine;
        lock (gate) mine = pending.Remove(id);
        return mine ? Fail(id, code, message) : await reply.Task.ConfigureAwait(false);
    }

    private static Reply Fail(int id, string code, string message) =>
        new(id, false, null, new BridgeError(code, message));

    private static TimeSpan Backoff(int attempt) =>
        TimeSpan.FromSeconds(Math.Min(MaxBackoff.TotalSeconds, 1 << Math.Min(attempt - 1, 5)));

    /// <summary>
    /// One bridge process and its write slot. Writes are one at a time per process, so a write stuck
    /// on a dead bridge never blocks the next bridge. <see cref="End"/> releases every waiter.
    /// </summary>
    [System.Diagnostics.CodeAnalysis.SuppressMessage("Design", "CA1001", Justification = "Never disposed on purpose: requests may still link to Ended, and a write continuation may release writeSlot after the bridge ended.")]
    private sealed class Link(IBridgeProcess process)
    {
        private readonly SemaphoreSlim writeSlot = new(1, 1);

        // Never disposed: requests may still link to its token after the bridge ended.
        private readonly CancellationTokenSource ended = new();

        /// <summary>Guarded by the client's gate.</summary>
        public bool Connected { get; set; }

        public CancellationToken Ended => ended.Token;

        public void End() => ended.Cancel();

        /// <summary>
        /// Waits for the write slot until <paramref name="stopWaiting"/> fires (request timeout or
        /// bridge end), never writes after that, and keeps the slot until the write itself finishes.
        /// </summary>
        public async Task WriteAsync(string line, CancellationToken stopWaiting)
        {
            await writeSlot.WaitAsync(stopWaiting).ConfigureAwait(false);
            Task write;
            try
            {
                stopWaiting.ThrowIfCancellationRequested();
                write = process.WriteLineAsync(line);
            }
            catch
            {
                writeSlot.Release();
                throw;
            }

            Observe(write);
            _ = write.ContinueWith(
                _ => writeSlot.Release(),
                CancellationToken.None,
                TaskContinuationOptions.ExecuteSynchronously,
                TaskScheduler.Default);
            await write.WaitAsync(stopWaiting).ConfigureAwait(false);
        }
    }
}

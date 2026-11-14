using System.Text.Json;

namespace SessionHub.Core.Settings;

/// <summary>
/// Reads and writes <c>gui.json</c> in one directory. Writes go to a temp file that replaces the
/// target, so a crash never leaves half a file. A missing or corrupt file loads as defaults; a corrupt
/// one is first renamed to <c>gui.json.bad</c> (timestamped when that exists), so the next save
/// cannot overwrite drafts someone may still recover, and <see cref="Corrupt"/> is raised. Those copies
/// are deleted <see cref="BadCopyRetention"/> after the move, on a later load or write. A failed
/// write keeps its text pending and is tried again every <see cref="RetryDelay"/>; it never ends the tray.
/// </summary>
public sealed class SettingsStore : IDisposable
{
    public const string FileName = "gui.json";
    public static readonly TimeSpan DefaultDebounce = TimeSpan.FromMilliseconds(500);

    /// <summary>A failed write is tried again after this long.</summary>
    public static readonly TimeSpan RetryDelay = TimeSpan.FromSeconds(5);

    /// <summary><c>gui.json.bad*</c> copies older than this are deleted when the store loads or writes.</summary>
    public static readonly TimeSpan BadCopyRetention = TimeSpan.FromDays(7);

    private static readonly JsonSerializerOptions Options = new(JsonSerializerDefaults.Web) { WriteIndented = true };

    private readonly string dir;
    private readonly TimeSpan debounce;
    private readonly TimeProvider time;
    private readonly object gate = new();
    private readonly object writeLock = new();
    private readonly ITimer timer;
    private string? pending;
    private bool disposed;

    [ThreadStatic]
    private static bool inEvent;

    public SettingsStore(string directory, TimeSpan? debounce = null, TimeProvider? time = null)
    {
        ArgumentException.ThrowIfNullOrEmpty(directory);
        dir = directory;
        this.debounce = debounce ?? DefaultDebounce;
        this.time = time ?? TimeProvider.System;
        timer = this.time.CreateTimer(_ => OnTimer(), null, Timeout.InfiniteTimeSpan, Timeout.InfiniteTimeSpan);
    }

    /// <summary>Raised after a write succeeded.</summary>
    public event Action? Saved;

    /// <summary>Raised when a write failed. The text stays pending and is tried again after <see cref="RetryDelay"/>.</summary>
    public event Action<Exception>? SaveFailed;

    /// <summary>
    /// Raised by <see cref="Load"/> when <c>gui.json</c> could not be parsed: the error, and the path it
    /// was moved to (null when the rename failed). Defaults are loaded either way.
    /// </summary>
    public event Action<Exception, string?>? Corrupt;

    /// <summary><c>%LOCALAPPDATA%\SessionHub</c>: the environment variable first, then the shell folder.</summary>
    /// <param name="env">Reads an environment variable; the process environment by default.</param>
    public static string DefaultDirectory(Func<string, string?>? env = null)
    {
        var local = (env ?? Environment.GetEnvironmentVariable).Invoke("LOCALAPPDATA");
        if (string.IsNullOrEmpty(local)) local = Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData);
        return Path.Combine(local, "SessionHub");
    }

    public GuiSettings Load()
    {
        var path = Path.Combine(dir, FileName);
        DeleteOldBadCopies();
        string text;
        try
        {
            if (!File.Exists(path)) return new GuiSettings();
            text = File.ReadAllText(path);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or NotSupportedException)
        {
            // Unreadable now (locked, no access): defaults, and the file is left as it is.
            return new GuiSettings();
        }

        try
        {
            var s = JsonSerializer.Deserialize<GuiSettings>(text, Options);
            return s is null ? new GuiSettings() : Normalize(s);
        }
        catch (Exception e) when (e is JsonException or NotSupportedException)
        {
            Corrupt?.Invoke(e, MoveAside(path));
            return new GuiSettings();
        }
    }

    /// <summary>Renames a corrupt file to <c>gui.json.bad</c>, or a timestamped name when that exists. Null on failure.</summary>
    private string? MoveAside(string path)
    {
        try
        {
            var bad = path + ".bad";
            if (File.Exists(bad))
            {
                bad = string.Create(
                    System.Globalization.CultureInfo.InvariantCulture,
                    $"{path}.bad-{time.GetUtcNow():yyyyMMdd-HHmmss-fff}");
            }

            File.Move(path, bad);
            TouchMoved(bad);
            return bad;
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            return null;
        }
    }

    /// <summary>
    /// A move keeps the file's write time, so the copy's age is restarted here: it is kept for
    /// <see cref="BadCopyRetention"/> from the move, not from the last write of the corrupt file.
    /// </summary>
    private void TouchMoved(string bad)
    {
        try
        {
            File.SetLastWriteTimeUtc(bad, time.GetUtcNow().UtcDateTime);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            // The copy then ages from its old write time; it still holds the text until it is deleted.
        }
    }

    /// <summary>
    /// Deletes <c>gui.json.bad*</c> copies last written more than <see cref="BadCopyRetention"/> ago. They
    /// may hold old drafts. Never touches <c>gui.json</c> itself; a copy that can not be read or deleted
    /// now is left for the next try.
    /// </summary>
    private void DeleteOldBadCopies()
    {
        var prefix = FileName + ".bad";
        var cutoff = (time.GetUtcNow() - BadCopyRetention).UtcDateTime;
        IEnumerable<string> files;
        try
        {
            if (!Directory.Exists(dir)) return;
            files = Directory.EnumerateFiles(dir, prefix + "*").ToList();
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            return;
        }

        foreach (var file in files)
        {
            if (!Path.GetFileName(file).StartsWith(prefix, StringComparison.OrdinalIgnoreCase)) continue;
            try
            {
                if (File.GetLastWriteTimeUtc(file) < cutoff) File.Delete(file);
            }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException)
            {
                // Locked or no access: try again on the next load or write.
            }
        }
    }

    /// <summary>
    /// Writes now. Replaces anything still pending, so an older scheduled save never lands after it.
    /// Does nothing after <see cref="Dispose"/>. Called from inside a <see cref="Saved"/> or
    /// <see cref="SaveFailed"/> handler on the same thread, it only schedules, so a failing write can not recurse.
    /// </summary>
    public void Save(GuiSettings settings)
    {
        if (inEvent)
        {
            ScheduleSave(settings);
            return;
        }

        var json = Serialize(settings);
        lock (gate)
        {
            if (disposed) return;
            pending = json;
            timer.Change(Timeout.InfiniteTimeSpan, Timeout.InfiniteTimeSpan);
        }

        Flush();
    }

    /// <summary>Remembers the settings (as text, so later changes to the object do not leak in) and writes after the debounce.</summary>
    public void ScheduleSave(GuiSettings settings)
    {
        var json = Serialize(settings);
        lock (gate)
        {
            if (disposed) return;
            pending = json;
            timer.Change(debounce, Timeout.InfiniteTimeSpan);
        }
    }

    /// <summary>Writes a pending save now. Call on exit.</summary>
    public void Flush()
    {
        // The pending text is taken inside the write lock, so writes land in the order they were made.
        // The gate is never held during file IO, and events are raised after both locks are released.
        Exception? failure = null;
        bool wrote;
        lock (writeLock)
        {
            string? json;
            lock (gate)
            {
                json = pending;
                pending = null;
            }

            if (json is null) return;
            try
            {
                Write(json);
            }
            catch (Exception e) when (e is not OutOfMemoryException)
            {
                failure = e;
            }

            wrote = failure is null;
            if (!wrote)
            {
                lock (gate)
                {
                    // A save scheduled during the write is newer and keeps its own debounce.
                    if (pending is null)
                    {
                        pending = json;
                        if (!disposed) timer.Change(RetryDelay, Timeout.InfiniteTimeSpan);
                    }
                }
            }
        }

        Raise(wrote, failure);
    }

    private void OnTimer()
    {
        try
        {
            Flush();
        }
        catch (Exception e) when (e is not OutOfMemoryException)
        {
            // Flush reports write failures itself; nothing may escape a timer callback.
        }
    }

    private void Raise(bool wrote, Exception? failure)
    {
        inEvent = true;
        try
        {
            if (wrote) Saved?.Invoke();
            else SaveFailed?.Invoke(failure!);
        }
        catch (Exception e) when (e is not OutOfMemoryException)
        {
            // A handler that throws must never end a timer callback or the tray.
        }
        finally
        {
            inEvent = false;
        }
    }

    /// <summary>Writes a pending save, then stops the timer.</summary>
    public void Dispose()
    {
        lock (gate)
        {
            if (disposed) return;
            disposed = true;
        }

        Flush();
        timer.Dispose();
    }

    private static string Serialize(GuiSettings settings) => JsonSerializer.Serialize(Normalize(settings), Options);

    private static GuiSettings Normalize(GuiSettings s)
    {
        var drafts = new Dictionary<string, string>();
        foreach (var (key, text) in s.Drafts ?? [])
        {
            if (text is null) continue;
            drafts[key] = CapDraft(text);
        }

        // Seen times are kept only for drafts that exist.
        var seen = new Dictionary<string, DateTimeOffset>();
        foreach (var (key, at) in s.DraftsSeen ?? [])
        {
            if (drafts.ContainsKey(key)) seen[key] = at;
        }

        // Dismissed keys: no blanks, no repeats, none too long, and only the newest MaxDismissed.
        var dismissed = new List<string>();
        foreach (var key in s.Dismissed ?? [])
        {
            if (string.IsNullOrWhiteSpace(key) || key.Length > GuiSettings.MaxDismissedKeyChars || dismissed.Contains(key)) continue;
            dismissed.Add(key);
        }

        if (dismissed.Count > GuiSettings.MaxDismissed) dismissed.RemoveRange(0, dismissed.Count - GuiSettings.MaxDismissed);

        return new GuiSettings
        {
            Notifications = s.Notifications,
            Window = s.Window,
            Drafts = drafts,
            DraftsSeen = seen,
            Dismissed = dismissed,
        };
    }

    /// <summary>Cuts a draft to <see cref="GuiSettings.MaxDraftChars"/> without splitting a surrogate pair.</summary>
    private static string CapDraft(string text)
    {
        var max = GuiSettings.MaxDraftChars;
        if (text.Length <= max) return text;
        if (char.IsHighSurrogate(text[max - 1])) max--;
        return text[..max];
    }

    private void Write(string json)
    {
        Directory.CreateDirectory(dir);
        var target = Path.Combine(dir, FileName);
        var temp = Path.Combine(dir, FileName + "." + Guid.NewGuid().ToString("N") + ".tmp");
        try
        {
            File.WriteAllText(temp, json);
            if (File.Exists(target)) File.Replace(temp, target, null);
            else File.Move(temp, target);
        }
        finally
        {
            if (File.Exists(temp)) File.Delete(temp);
        }

        DeleteOldBadCopies();
    }
}

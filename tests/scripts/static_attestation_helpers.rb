# frozen_string_literal: true

require "digest"
require "json"
require "pathname"

module StaticAttestation
  module_function

  class Failure < StandardError
    attr_reader :check, :input_path, :expected, :actual, :reason

    def initialize(message, check:, input_path: nil, expected: nil, actual: nil, reason: nil)
      super(message)
      @check = check
      @input_path = input_path
      @expected = expected
      @actual = actual
      @reason = reason || message
    end
  end

  @log_io = $stderr
  @log_enabled = ENV.fetch("STATIC_ATTESTATION_LOGS", "1") != "0"

  def configure(log_io: nil, log_enabled: nil)
    @log_io = log_io unless log_io.nil?
    @log_enabled = log_enabled unless log_enabled.nil?
  end

  def repo_root
    Pathname.new(ENV.fetch("FRANKENTERM_REPO_ROOT", Dir.pwd)).realpath
  end

  def assert!(condition, message, check: "assert", input_path: nil, expected: true, actual: condition)
    if condition
      log_check(check, input_path: input_path, expected: expected, actual: actual, status: "pass")
      return true
    end

    fail!(message, check: check, input_path: input_path, expected: expected, actual: actual)
  end

  def fail!(message, check:, input_path: nil, expected: nil, actual: nil, reason: message)
    log_check(
      check,
      input_path: input_path,
      expected: expected,
      actual: actual,
      status: "fail",
      reason: reason,
    )
    raise Failure.new(
      message,
      check: check,
      input_path: input_path,
      expected: expected,
      actual: actual,
      reason: reason,
    )
  end

  def expect_failure!(description, check:)
    yield
  rescue Failure => error
    log_check(
      check,
      input_path: description,
      expected: "StaticAttestation::Failure",
      actual: error.class.name,
      status: "pass",
      reason: error.reason,
    )
    error
  else
    fail!(
      "expected static attestation failure did not occur: #{description}",
      check: check,
      input_path: description,
      expected: "StaticAttestation::Failure",
      actual: "no_failure",
    )
  end

  def log_check(check, input_path:, expected:, actual:, status:, reason: nil)
    return unless @log_enabled

    @log_io.puts(
      JSON.generate(
        {
          "check" => check.to_s,
          "input_path" => input_path,
          "expected" => safe_value(expected),
          "actual" => safe_value(actual),
          "status" => status,
          "failure_reason" => reason,
        }.compact,
      ),
    )
  end

  def repo_relative_path!(path, field: "path", check: "repo_relative_path")
    unless path.is_a?(String) && !path.empty?
      fail!("#{field} must be a non-empty string", check: check, input_path: path, expected: "repo-relative path", actual: path)
    end
    if path.start_with?("/")
      fail!("#{field} must be repo-relative: #{path}", check: check, input_path: path, expected: "relative", actual: "absolute")
    end
    if path.include?("\0")
      fail!("#{field} must not contain NUL bytes", check: check, input_path: path, expected: "no NUL", actual: "contains NUL")
    end
    if path.split("/").any? { |part| part == ".." }
      fail!("#{field} must not contain parent traversal: #{path}", check: check, input_path: path, expected: "no .. component", actual: path)
    end

    log_check(check, input_path: path, expected: "repo-relative", actual: "repo-relative", status: "pass")
    path
  end

  def repo_path(path, root: repo_root)
    relative = repo_relative_path!(path)
    candidate = root.join(relative).cleanpath
    root_s = root.to_s
    candidate_s = candidate.to_s
    unless candidate_s == root_s || candidate_s.start_with?("#{root_s}#{File::SEPARATOR}")
      fail!(
        "path escapes repository root: #{path}",
        check: "repo_path_boundary",
        input_path: path,
        expected: root_s,
        actual: candidate_s,
      )
    end
    candidate
  end

  def require_file!(path, check: "file_exists")
    candidate = repo_path(path)
    actual = if File.file?(candidate)
      "file"
    elsif File.exist?(candidate)
      "not_file"
    else
      "missing"
    end
    assert!(
      File.file?(candidate),
      "missing file: #{path}",
      check: check,
      input_path: path,
      expected: "file",
      actual: actual,
    )
    candidate
  end

  def read_text!(path, check: "read_text")
    candidate = require_file!(path, check: check)
    File.binread(candidate)
  end

  def read_json!(path, check: "read_json")
    JSON.parse(read_text!(path, check: check))
  rescue JSON::ParserError => error
    fail!(
      "#{path} does not parse as JSON: #{error.message}",
      check: check,
      input_path: path,
      expected: "valid_json",
      actual: error.message,
    )
  end

  def expected_strings(*values)
    values.flatten(1).map do |value|
      unless value.is_a?(String) && !value.empty?
        fail!("expected string must be non-empty", check: "expected_string", expected: "non-empty string", actual: value)
      end
      value
    end.freeze
  end

  def require_terms!(text, terms, source:, check: "required_terms")
    expected_strings(terms).each do |term|
      assert!(
        text.include?(term),
        "#{source} missing #{term.inspect}",
        check: check,
        input_path: source,
        expected: term,
        actual: text.include?(term) ? "present" : "missing",
      )
    end
  end

  def require_file_terms!(path, terms, check: "required_file_terms")
    require_terms!(read_text!(path), terms, source: path, check: check)
  end

  def require_source_documents!(paths, check: "source_documents")
    expected_strings(paths).each { |path| require_file!(path, check: check) }
  end

  def require_seed_corpus!(corpus_dir, seeds:, name_key: "name", bytes_key: "bytes", check: "seed_corpus")
    corpus_path = repo_path(corpus_dir)
    corpus_actual = if File.directory?(corpus_path)
      "directory"
    elsif File.exist?(corpus_path)
      "not_directory"
    else
      "missing"
    end
    assert!(
      File.directory?(corpus_path),
      "missing seed corpus directory: #{corpus_dir}",
      check: check,
      input_path: corpus_dir,
      expected: "directory",
      actual: corpus_actual,
    )

    actual_sizes = Dir.children(corpus_path).sort.to_h do |name|
      child = corpus_path.join(name)
      entry_type = if File.file?(child)
        "file"
      elsif File.directory?(child)
        "directory"
      else
        "other"
      end
      assert!(
        File.file?(child),
        "seed corpus entry is not a file: #{corpus_dir}/#{name}",
        check: "#{check}.entry_type",
        input_path: "#{corpus_dir}/#{name}",
        expected: "file",
        actual: entry_type,
      )
      [name, File.size(child)]
    end
    declared_sizes = seeds.to_h do |seed|
      name = seed.fetch(name_key)
      bytes = seed.fetch(bytes_key)
      assert!(
        name.is_a?(String) && !name.empty?,
        "seed name must be a non-empty string",
        check: "#{check}.declared_name",
        input_path: corpus_dir,
        expected: "non-empty string",
        actual: name,
      )
      assert!(
        bytes.is_a?(Integer) && bytes >= 0,
        "seed byte count must be a non-negative integer for #{name}",
        check: "#{check}.declared_bytes",
        input_path: "#{corpus_dir}/#{name}",
        expected: "non-negative integer",
        actual: bytes,
      )
      [name, bytes]
    end

    assert!(
      declared_sizes.keys.sort == actual_sizes.keys.sort,
      "seed names do not match corpus files",
      check: "#{check}.names",
      input_path: corpus_dir,
      expected: declared_sizes.keys.sort,
      actual: actual_sizes.keys.sort,
    )
    assert!(
      declared_sizes == actual_sizes,
      "seed byte counts do not match corpus files",
      check: "#{check}.bytes",
      input_path: corpus_dir,
      expected: declared_sizes,
      actual: actual_sizes,
    )

    { seed_count: actual_sizes.length, total_bytes: actual_sizes.values.sum, sizes: actual_sizes }
  end

  def require_direct_exec_script!(path, strict_mode: "set -euo pipefail", check: "direct_exec_script")
    text = read_text!(path, check: check)
    candidate = repo_path(path)
    first_line = text.lines.first.to_s.strip
    assert!(
      first_line.start_with?("#!"),
      "#{path} missing shebang",
      check: "#{check}.shebang",
      input_path: path,
      expected: "#!",
      actual: first_line.empty? ? "empty" : first_line,
    )
    assert!(
      File.executable?(candidate),
      "#{path} is not executable",
      check: "#{check}.executable",
      input_path: path,
      expected: "executable",
      actual: File.executable?(candidate) ? "executable" : "not_executable",
    )
    assert!(
      text.include?(strict_mode),
      "#{path} missing strict mode #{strict_mode.inspect}",
      check: "#{check}.strict_mode",
      input_path: path,
      expected: strict_mode,
      actual: text.include?(strict_mode) ? "present" : "missing",
    )
    true
  end

  def safe_value(value)
    case value
    when nil, true, false, Numeric
      value
    when String
      safe_string(value)
    when Array
      {
        "count" => value.length,
        "items" => value.first(12).map { |item| safe_value(item) },
        "truncated" => value.length > 12,
      }
    when Hash
      value.keys.sort_by(&:to_s).to_h { |key| [key.to_s, safe_value(value[key])] }
    else
      safe_string(value.inspect)
    end
  end

  def safe_string(value)
    if safe_literal?(value)
      value
    else
      {
        "redacted" => "sha256:#{Digest::SHA256.hexdigest(value)}",
        "bytes" => value.bytesize,
      }
    end
  end

  def safe_literal?(value)
    value.bytesize <= 120 &&
      !value.match?(/[[:cntrl:]]/) &&
      value.match?(/\A[-A-Za-z0-9_.,:\/@+=\[\]{}()#! ]*\z/) &&
      !value.match?(/(secret|token|password|api[_-]?key|pane content)/i)
  end
end

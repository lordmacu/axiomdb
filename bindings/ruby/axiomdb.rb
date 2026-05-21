# frozen_string_literal: true

# AxiomDB Ruby binding — Fiddle FFI over libaxiomdb_embedded.
#
# Uses Fiddle (Ruby stdlib, no gem needed) so it works on the system Ruby.
# Result materialization uses the packed-buffer path: one FFI call returns the
# whole result as a single buffer, parsed once in Ruby. Fiddle calls carry
# overhead (like ctypes/koffi), so collapsing ~2 calls/cell into one matters.
#
#   require_relative 'axiomdb'
#   db = AxiomDB.new('./myapp.db')
#   db.execute("CREATE TABLE users (id INT, name TEXT)")
#   db.execute("INSERT INTO users VALUES (1, 'Alice')")
#   db.query_tuples("SELECT * FROM users")  # [[1, "Alice"]]
#   db.query("SELECT * FROM users")         # [{"id"=>1, "name"=>"Alice"}]
#   db.close

require 'fiddle'
require 'fiddle/import'

class AxiomDBError < StandardError; end

module AxiomDBNative
  extend Fiddle::Importer

  def self.lib_path
    ext = case RbConfig::CONFIG['host_os']
          when /darwin/ then 'dylib'
          when /mswin|mingw/ then 'dll'
          else 'so'
          end
    name = "libaxiomdb_embedded.#{ext}"
    here = File.dirname(File.expand_path(__FILE__))
    [
      File.join(here, name),
      File.join(here, '..', '..', 'target', 'release', name),
      File.join(here, '..', '..', 'target', 'debug', name),
      "/usr/local/lib/#{name}"
    ].find { |p| File.exist?(p) } || name
  end

  dlload lib_path
  extern 'void* axiomdb_open(const char*)'
  extern 'long long axiomdb_execute(void*, const char*)'
  extern 'void* axiomdb_query_packed(void*, const char*, void*)'
  extern 'void axiomdb_packed_free(void*, size_t)'
  extern 'void axiomdb_close(void*)'
  extern 'char* axiomdb_last_error(void*)'
end

class AxiomDB
  PACKED_MAGIC = 0x41584D31

  # Open or create a database at +path+ (":memory:" for ephemeral).
  def initialize(path)
    @ptr = AxiomDBNative.axiomdb_open(path)
    raise AxiomDBError, "failed to open database at #{path.inspect}" if @ptr.null?
  end

  # Execute a DDL/DML statement. Returns rows affected.
  def execute(sql)
    check_open
    n = AxiomDBNative.axiomdb_execute(@ptr, sql)
    raise AxiomDBError, last_error || 'execute failed' if n < 0

    n
  end

  # Execute a SELECT, returning rows as arrays (fastest path).
  def query_tuples(sql)
    query_packed(sql)[1]
  end

  # Execute a SELECT, returning rows as hashes (column name => value).
  def query(sql)
    cols, rows = query_packed(sql)
    rows.map { |row| cols.zip(row).to_h }
  end

  # Execute a SELECT, returning [column_names, rows-as-arrays].
  def query_with_columns(sql)
    query_packed(sql)
  end

  def begin_txn
    execute('BEGIN')
  end

  def commit
    execute('COMMIT')
  end

  def rollback
    execute('ROLLBACK')
  end

  # Return the last error message, or nil.
  def last_error
    return nil if @ptr.nil? || @ptr.null?

    msg = AxiomDBNative.axiomdb_last_error(@ptr)
    msg.null? ? nil : msg.to_s
  end

  # Close the database. Safe to call multiple times.
  def close
    return if @ptr.nil?

    AxiomDBNative.axiomdb_close(@ptr)
    @ptr = nil
  end

  private

  def check_open
    raise AxiomDBError, 'database is closed' if @ptr.nil? || @ptr.null?
  end

  # One FFI call returns the whole result; copy it into a Ruby string, free the
  # native buffer, then parse. Returns [columns, rows].
  def query_packed(sql)
    check_open
    len_ptr = Fiddle::Pointer.malloc(Fiddle::SIZEOF_SIZE_T, Fiddle::RUBY_FREE)
    buf_ptr = AxiomDBNative.axiomdb_query_packed(@ptr, sql, len_ptr)
    raise AxiomDBError, last_error || 'query failed' if buf_ptr.null?

    len = len_ptr[0, Fiddle::SIZEOF_SIZE_T].unpack1('Q<')
    data = buf_ptr[0, len] # copy native bytes into a Ruby string
    AxiomDBNative.axiomdb_packed_free(buf_ptr, len)
    parse_packed(data)
  end

  # Decode the packed buffer. Ruby Integer is arbitrary-precision, so i64 is
  # exact (no BigInt workaround like JS/Python need).
  def parse_packed(buf)
    off = 0
    magic = buf.byteslice(off, 4).unpack1('L<'); off += 4
    raise AxiomDBError, 'corrupt packed buffer' unless magic == PACKED_MAGIC

    ncols = buf.byteslice(off, 4).unpack1('L<'); off += 4
    nrows = buf.byteslice(off, 8).unpack1('Q<'); off += 8

    cols = Array.new(ncols)
    ncols.times do |c|
      len = buf.byteslice(off, 4).unpack1('L<'); off += 4
      cols[c] = buf.byteslice(off, len).force_encoding('UTF-8'); off += len
    end

    rows = Array.new(nrows)
    nrows.times do |r|
      row = Array.new(ncols)
      ncols.times do |c|
        tag = buf.getbyte(off); off += 1
        case tag
        when 1 # INT
          row[c] = buf.byteslice(off, 8).unpack1('q<'); off += 8
        when 3 # TEXT
          len = buf.byteslice(off, 4).unpack1('L<'); off += 4
          row[c] = buf.byteslice(off, len).force_encoding('UTF-8'); off += len
        when 2 # REAL
          row[c] = buf.byteslice(off, 8).unpack1('E'); off += 8
        when 4 # BLOB
          len = buf.byteslice(off, 4).unpack1('L<'); off += 4
          row[c] = buf.byteslice(off, len); off += len # binary string
        else # NULL
          row[c] = nil
        end
      end
      rows[r] = row
    end
    [cols, rows]
  end
end
